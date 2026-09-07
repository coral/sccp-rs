//! Exclusive bridge-session ownership with independent bridge and call lanes.

use std::collections::{HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, RwLock};

use tokio::task::{AbortHandle, JoinSet};

use super::{
    Access, AsteriskBackend, AsteriskBackendError, AsteriskChannel, BargeBridgeSession,
    BargeOperation, BridgeOperation, BridgeSession, CallFeatureProvider as _, NonNull, PbxBridgeId,
    PbxCallId, RwLockExt as _, TransferCompletion, native_channel, with_channels,
    with_two_channels,
};
use crate::runtime::mailbox::RUNTIME_MAILBOX_CAPACITY;
use crate::runtime::owner::{
    EffectExecutor, OperationId, RuntimeHandle, RuntimeOwner, RuntimeState, Step,
};

type Outcome = Result<(), AsteriskBackendError>;

#[derive(Clone)]
pub(super) struct BridgeHandle {
    runtime: RuntimeHandle<BridgeCommand, Outcome>,
    #[cfg(feature = "telemetry")]
    snapshot: Arc<RwLock<Arc<BridgeSnapshot>>>,
}

pub(super) enum BridgeAction {
    Bridge(BridgeOperation),
    Barge(BargeOperation),
    Transfer(TransferCompletion),
    Drain,
}

pub(super) struct BridgeCommand {
    access: Access,
    action: BridgeAction,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(super) enum BridgeResource {
    Bridge(PbxBridgeId),
    Call(PbxCallId),
}

#[derive(Clone, Default)]
struct BridgeSnapshot {
    bridges: HashSet<PbxBridgeId>,
    barges: HashSet<PbxBridgeId>,
}

#[derive(Default)]
pub(super) struct BridgeState {
    snapshot: Arc<RwLock<Arc<BridgeSnapshot>>>,
    bridges: HashMap<PbxBridgeId, BridgeSession>,
    barges: HashMap<PbxBridgeId, BargeBridgeSession>,
    calls: HashMap<PbxBridgeId, HashSet<PbxCallId>>,
    intents: HashMap<OperationId, (Option<PbxBridgeId>, Vec<PbxCallId>)>,
    retiring_after: Option<OperationId>,
}

pub(super) struct BridgeEffect {
    command: BridgeCommand,
    bridge: Option<BridgeSession>,
    barge: Option<BargeBridgeSession>,
    draining: Option<BridgeDrain>,
}

struct BridgeDrain {
    bridges: HashMap<PbxBridgeId, BridgeSession>,
    barges: HashMap<PbxBridgeId, BargeBridgeSession>,
}

pub(super) struct BridgeCompletion {
    bridge_id: Option<PbxBridgeId>,
    bridge: Option<BridgeSession>,
    barge: Option<BargeBridgeSession>,
    result: Outcome,
}

pub(super) struct BridgeExecutor;

impl BridgeAction {
    fn bridge_id(&self) -> Option<PbxBridgeId> {
        match self {
            Self::Bridge(operation) => Some(match operation {
                BridgeOperation::Create { bridge_id }
                | BridgeOperation::Destroy { bridge_id }
                | BridgeOperation::AddParticipant { bridge_id, .. }
                | BridgeOperation::RemoveParticipant { bridge_id, .. }
                | BridgeOperation::MergeConsultation { bridge_id, .. }
                | BridgeOperation::MergeCalls { bridge_id, .. }
                | BridgeOperation::MergeParticipant { bridge_id, .. }
                | BridgeOperation::SetParticipantMuted { bridge_id, .. }
                | BridgeOperation::SetParticipantMusicOnHold { bridge_id, .. }
                | BridgeOperation::RemoveConferenceParticipant { bridge_id, .. } => *bridge_id,
            }),
            Self::Barge(
                BargeOperation::Join { bridge_id, .. } | BargeOperation::Leave { bridge_id, .. },
            ) => Some(*bridge_id),
            Self::Transfer(_) | Self::Drain => None,
        }
    }

    fn call_ids(&self) -> Vec<PbxCallId> {
        match self {
            Self::Transfer(transfer) => vec![
                transfer.source.pbx_call_id,
                transfer.consultation.pbx_call_id,
            ],
            Self::Bridge(BridgeOperation::Create { .. } | BridgeOperation::Destroy { .. })
            | Self::Drain => Vec::new(),
            Self::Bridge(
                BridgeOperation::AddParticipant { call_id, .. }
                | BridgeOperation::RemoveParticipant { call_id, .. }
                | BridgeOperation::MergeParticipant { call_id, .. }
                | BridgeOperation::SetParticipantMuted { call_id, .. }
                | BridgeOperation::SetParticipantMusicOnHold { call_id, .. }
                | BridgeOperation::RemoveConferenceParticipant { call_id, .. },
            ) => vec![*call_id],
            Self::Bridge(BridgeOperation::MergeConsultation {
                original_call_id,
                consultation_call_id,
                ..
            }) => vec![*original_call_id, *consultation_call_id],
            Self::Bridge(BridgeOperation::MergeCalls { call_ids, .. }) => call_ids.clone(),
            Self::Barge(BargeOperation::Join {
                target_call_id,
                barger_call_id,
                ..
            }) => vec![*target_call_id, *barger_call_id],
            Self::Barge(BargeOperation::Leave { barger_call_id, .. }) => vec![*barger_call_id],
        }
    }
}

fn unavailable() -> AsteriskBackendError {
    AsteriskBackendError::Failed {
        operation: "bridge owner operation",
        calls: "runtime unavailable".into(),
    }
}

impl BridgeHandle {
    pub(super) fn snapshot(&self) -> crate::runtime::mailbox::QueueSnapshot {
        self.runtime.snapshot()
    }

    #[cfg(feature = "telemetry")]
    pub(super) fn snapshot_keys(&self) -> (Vec<PbxBridgeId>, Vec<PbxBridgeId>) {
        let snapshot = self.snapshot.read_unpoisoned();
        let mut bridges = snapshot.bridges.iter().copied().collect::<Vec<_>>();
        let mut barges = snapshot.barges.iter().copied().collect::<Vec<_>>();
        bridges.sort_unstable();
        barges.sort_unstable();
        (bridges, barges)
    }

    pub(super) fn execute(&self, access: &Access, action: BridgeAction) -> Outcome {
        self.runtime
            .try_request(
                BridgeCommand {
                    access: access.clone(),
                    action,
                },
                None,
            )
            .map_err(|_| unavailable())?
            .recv()
            .map_err(|_| unavailable())?
            .map_err(|_| unavailable())?
    }

    pub(super) async fn execute_async(&self, access: &Access, action: BridgeAction) -> Outcome {
        self.runtime
            .request(
                BridgeCommand {
                    access: access.clone(),
                    action,
                },
                None,
            )
            .await
            .map_err(|_| unavailable())?
    }

    pub(super) fn close(&self) {
        self.runtime.close();
    }
}

pub(super) fn bridge_runtime() -> (BridgeHandle, RuntimeOwner<BridgeState, BridgeExecutor>) {
    let snapshot = Arc::new(RwLock::new(Arc::new(BridgeSnapshot::default())));
    let (runtime, owner) = RuntimeOwner::new(
        BridgeState {
            snapshot: Arc::clone(&snapshot),
            ..BridgeState::default()
        },
        BridgeExecutor,
        RUNTIME_MAILBOX_CAPACITY,
    );
    (
        BridgeHandle {
            runtime,
            #[cfg(feature = "telemetry")]
            snapshot,
        },
        owner,
    )
}

impl RuntimeState for BridgeState {
    type Command = BridgeCommand;
    type Reply = Outcome;
    type Resource = BridgeResource;
    type Effect = BridgeEffect;
    type Completion = BridgeCompletion;

    fn resources(&self, command: &BridgeCommand) -> Vec<BridgeResource> {
        let mut resources = command
            .action
            .call_ids()
            .into_iter()
            .map(BridgeResource::Call)
            .collect::<Vec<_>>();
        match command.action.bridge_id() {
            Some(bridge_id) => {
                resources.push(BridgeResource::Bridge(bridge_id));
                for (candidate, calls) in self.intents.values() {
                    if *candidate == Some(bridge_id) {
                        resources.extend(calls.iter().copied().map(BridgeResource::Call));
                    }
                }
                if let Some(calls) = self.calls.get(&bridge_id) {
                    resources.extend(calls.iter().copied().map(BridgeResource::Call));
                }
            }
            None if matches!(command.action, BridgeAction::Drain) => {
                for (bridge, calls) in self.intents.values() {
                    resources.extend(bridge.map(BridgeResource::Bridge));
                    resources.extend(calls.iter().copied().map(BridgeResource::Call));
                }
                resources.extend(self.calls.keys().copied().map(BridgeResource::Bridge));
                resources.extend(
                    self.calls
                        .values()
                        .flatten()
                        .copied()
                        .map(BridgeResource::Call),
                );
            }
            None => {}
        }
        resources
    }

    fn admit(&mut self, id: OperationId, command: &BridgeCommand) -> Vec<BridgeResource> {
        if matches!(command.action, BridgeAction::Drain) {
            self.retiring_after = Some(id);
        }
        self.intents
            .insert(id, (command.action.bridge_id(), command.action.call_ids()));
        self.resources(command)
    }

    fn expired(&mut self, id: OperationId) {
        self.intents.remove(&id);
    }

    fn prepare(&mut self, id: OperationId, command: BridgeCommand) -> Step<Outcome, BridgeEffect> {
        if self.retiring_after.is_some_and(|retiring| id > retiring) {
            self.intents.remove(&id);
            return Step::Finished(Err(unavailable()));
        }
        let mut effect = BridgeEffect {
            command,
            bridge: None,
            barge: None,
            draining: None,
        };
        match effect.command.action.bridge_id() {
            Some(bridge_id) => {
                // Keep historical participants until session destruction. This is
                // conservative across native failures and partially applied merges.
                self.calls
                    .entry(bridge_id)
                    .or_default()
                    .extend(effect.command.action.call_ids());
                effect.bridge = self.bridges.remove(&bridge_id);
                effect.barge = self.barges.remove(&bridge_id);
            }
            None if matches!(effect.command.action, BridgeAction::Drain) => {
                self.calls.clear();
                *self.snapshot.write_unpoisoned() = Arc::new(BridgeSnapshot::default());
                effect.draining = Some(BridgeDrain {
                    bridges: std::mem::take(&mut self.bridges),
                    barges: std::mem::take(&mut self.barges),
                });
            }
            None => {}
        }
        Step::Effect(effect)
    }

    fn complete(
        &mut self,
        id: OperationId,
        completion: BridgeCompletion,
    ) -> Step<Outcome, BridgeEffect> {
        self.intents.remove(&id);
        {
            let mut snapshot = self.snapshot.write_unpoisoned();
            let snapshot = Arc::make_mut(&mut snapshot);
            match completion.bridge_id {
                Some(bridge_id) => {
                    if completion.bridge.is_some() {
                        snapshot.bridges.insert(bridge_id);
                    } else {
                        snapshot.bridges.remove(&bridge_id);
                    }
                    if completion.barge.is_some() {
                        snapshot.barges.insert(bridge_id);
                    } else {
                        snapshot.barges.remove(&bridge_id);
                    }
                }
                None => {}
            }
        }
        if let Some(bridge_id) = completion.bridge_id {
            match (completion.bridge, completion.barge) {
                (None, None) => {
                    self.calls.remove(&bridge_id);
                }
                (bridge, barge) => {
                    if let Some(bridge) = bridge {
                        self.bridges.insert(bridge_id, bridge);
                    }
                    if let Some(barge) = barge {
                        self.barges.insert(bridge_id, barge);
                    }
                }
            }
        }
        Step::Finished(completion.result)
    }

    fn failed(&mut self, id: OperationId) -> Step<Outcome, BridgeEffect> {
        self.intents.remove(&id);
        Step::Finished(Err(unavailable()))
    }
}

impl EffectExecutor<BridgeEffect, BridgeCompletion> for BridgeExecutor {
    fn spawn(
        &self,
        mut effect: BridgeEffect,
        workers: &mut JoinSet<BridgeCompletion>,
    ) -> AbortHandle {
        workers.spawn_blocking(move || {
            let bridge_id = effect.command.action.bridge_id();
            let result = catch_unwind(AssertUnwindSafe(|| effect.execute()))
                .unwrap_or_else(|_| Err(unavailable()));
            BridgeCompletion {
                bridge_id,
                bridge: effect.bridge,
                barge: effect.barge,
                result,
            }
        })
    }
}

impl BridgeEffect {
    fn execute(&mut self) -> Outcome {
        let backend = AsteriskBackend::new(&self.command.access);
        match &self.command.action {
            BridgeAction::Bridge(operation) => {
                execute_bridge(&backend, &mut self.bridge, operation)
            }
            BridgeAction::Barge(operation) => execute_barge(&backend, &mut self.barge, operation),
            BridgeAction::Transfer(transfer) => {
                let first = transfer.source.pbx_call_id;
                let second = transfer.consultation.pbx_call_id;
                let result = with_two_channels(backend.access, first, second, |first, second| {
                    NonNull::new(first)
                        .zip(NonNull::new(second))
                        .is_some_and(|(first, second)| {
                            matches!(
                                unsafe { native_channel::attended_transfer(first, second) },
                                native_channel::AttendedTransferResult::Success
                            )
                        })
                });
                if result == Some(true) {
                    Ok(())
                } else {
                    Err(AsteriskBackendError::Failed {
                        operation: "bridge transfer",
                        calls: format!("{} and {}", first.0, second.0),
                    })
                }
            }
            BridgeAction::Drain => {
                let mut result = Ok(());
                if let Some(state) = self.draining.take() {
                    for (_, bridge) in state.bridges {
                        let next = bridge.destroy().map_err(AsteriskBackendError::CallFeature);
                        if result.is_ok() {
                            result = next;
                        }
                    }
                    for (_, barge) in state.barges {
                        let next = barge.release().map_err(AsteriskBackendError::CallFeature);
                        if result.is_ok() {
                            result = next;
                        }
                    }
                }
                result
            }
        }
    }
}

fn execute_bridge(
    backend: &AsteriskBackend<'_>,
    session: &mut Option<BridgeSession>,
    operation: &BridgeOperation,
) -> Outcome {
    let bridge_id = BridgeAction::Bridge(operation.clone())
        .bridge_id()
        .ok_or_else(unavailable)?;
    match operation {
        BridgeOperation::Create { .. } => {
            if session.is_some() {
                return Err(AsteriskBackendError::BridgeConflict { bridge_id });
            }
            *session = Some(
                backend
                    .call_features
                    .create_bridge(bridge_id)
                    .map_err(AsteriskBackendError::CallFeature)?,
            );
            return Ok(());
        }
        BridgeOperation::Destroy { .. } => {
            return session
                .take()
                .ok_or(AsteriskBackendError::BridgeUnavailable {
                    operation: "destroy bridge",
                    bridge_id,
                })?
                .destroy()
                .map_err(AsteriskBackendError::CallFeature);
        }
        _ => {}
    }
    let bridge = session
        .as_mut()
        .ok_or(AsteriskBackendError::BridgeUnavailable {
            operation: "modify bridge",
            bridge_id,
        })?;
    match operation {
        BridgeOperation::Create { .. } | BridgeOperation::Destroy { .. } => {
            return Err(unavailable());
        }
        BridgeOperation::AddParticipant { call_id, .. } => {
            backend.with_call_feature_channel("add bridge participant", *call_id, |channel| {
                bridge
                    .add(channel)
                    .map_err(AsteriskBackendError::CallFeature)
            })
        }
        BridgeOperation::RemoveParticipant { call_id, .. } => {
            backend.with_call_feature_channel("remove bridge participant", *call_id, |channel| {
                bridge
                    .remove(channel)
                    .map_err(AsteriskBackendError::CallFeature)
            })
        }
        BridgeOperation::MergeParticipant { call_id, .. } => {
            backend.with_call_feature_channel("merge conference participant", *call_id, |channel| {
                bridge
                    .merge_participant(channel)
                    .map_err(AsteriskBackendError::CallFeature)
            })
        }
        BridgeOperation::SetParticipantMuted { call_id, muted, .. } => backend
            .with_call_feature_channel("set conference participant mute", *call_id, |channel| {
                bridge
                    .set_participant_muted(channel, *muted)
                    .map_err(AsteriskBackendError::CallFeature)
            }),
        BridgeOperation::SetParticipantMusicOnHold {
            call_id,
            class,
            enabled,
            ..
        } => backend.with_call_feature_channel(
            "set conference participant music on hold",
            *call_id,
            |channel| {
                bridge
                    .set_participant_music_on_hold(channel, class, *enabled)
                    .map_err(AsteriskBackendError::CallFeature)
            },
        ),
        BridgeOperation::RemoveConferenceParticipant { call_id, .. } => backend
            .with_call_feature_channel("remove conference participant", *call_id, |channel| {
                bridge
                    .remove_participant_and_hangup(channel)
                    .map_err(AsteriskBackendError::CallFeature)
            }),
        BridgeOperation::MergeConsultation {
            original_call_id,
            consultation_call_id,
            ..
        } => with_two_channels(
            backend.access,
            *original_call_id,
            *consultation_call_id,
            |original, consultation| {
                let original =
                    unsafe { AsteriskChannel::from_raw(original.cast()) }.map_err(|_| {
                        AsteriskBackendError::CallUnavailable {
                            operation: "merge conference consultation",
                            call_id: *original_call_id,
                        }
                    })?;
                let consultation = unsafe { AsteriskChannel::from_raw(consultation.cast()) }
                    .map_err(|_| AsteriskBackendError::CallUnavailable {
                        operation: "merge conference consultation",
                        call_id: *consultation_call_id,
                    })?;
                bridge
                    .merge_consultation(&original, &consultation)
                    .map_err(AsteriskBackendError::CallFeature)
            },
        )
        .unwrap_or(Err(AsteriskBackendError::CallUnavailable {
            operation: "merge conference consultation",
            call_id: *consultation_call_id,
        })),
        BridgeOperation::MergeCalls { call_ids, .. } => {
            with_channels(backend.access, call_ids, |channels| {
                let channels = channels
                    .iter()
                    .zip(call_ids)
                    .map(|(channel, call_id)| unsafe {
                        AsteriskChannel::from_raw(channel.cast()).map_err(|_| {
                            AsteriskBackendError::CallUnavailable {
                                operation: "merge selected conference calls",
                                call_id: *call_id,
                            }
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                bridge
                    .merge_calls(&channels)
                    .map_err(AsteriskBackendError::CallFeature)
            })
            .unwrap_or_else(|| Err(unavailable()))
        }
    }
}

fn execute_barge(
    backend: &AsteriskBackend<'_>,
    session: &mut Option<BargeBridgeSession>,
    operation: &BargeOperation,
) -> Outcome {
    match operation {
        BargeOperation::Join {
            bridge_id,
            target_call_id,
            barger_call_id,
        } => {
            let created = session.is_none();
            if created {
                *session = Some(backend.with_call_feature_channel(
                    "acquire barge bridge",
                    *target_call_id,
                    |channel| {
                        backend
                            .call_features
                            .acquire_barge_bridge(*bridge_id, channel)
                            .map_err(AsteriskBackendError::CallFeature)
                    },
                )?);
            }
            let result = backend.with_call_feature_channel(
                "add barge participant",
                *barger_call_id,
                |channel| {
                    session
                        .as_mut()
                        .ok_or_else(unavailable)?
                        .add(channel)
                        .map_err(AsteriskBackendError::CallFeature)
                },
            );
            if result.is_err() && created {
                if let Some(session) = session.take() {
                    let _ = session.release();
                }
            }
            result
        }
        BargeOperation::Leave {
            bridge_id,
            barger_call_id,
            last_participant,
        } => {
            let removal = backend.with_call_feature_channel(
                "remove barge participant",
                *barger_call_id,
                |channel| {
                    session
                        .as_mut()
                        .ok_or(AsteriskBackendError::BridgeUnavailable {
                            operation: "remove barge participant",
                            bridge_id: *bridge_id,
                        })?
                        .remove(channel)
                        .map_err(AsteriskBackendError::CallFeature)
                },
            );
            let release = if *last_participant {
                session
                    .take()
                    .ok_or(AsteriskBackendError::BridgeUnavailable {
                        operation: "release barge bridge",
                        bridge_id: *bridge_id,
                    })?
                    .release()
                    .map_err(AsteriskBackendError::CallFeature)
            } else {
                Ok(())
            };
            removal.and(release)
        }
    }
}
