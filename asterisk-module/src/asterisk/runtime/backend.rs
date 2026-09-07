use super::media::admit_clear_audio_media;
use super::media::{retarget_to_anchor, retarget_to_direct};
use super::{
    Access, AmiEventError, AnnouncementAdapter, AnnouncementCall, AnnouncementFailureStage,
    AnnouncementGeneration, Arc, AsteriskCallFeatures, AsteriskChannel, AsteriskChannelMetadata,
    AsteriskDatabase, AsteriskHints, AsteriskPartyUpdates, BargeOperation, BridgeOperation,
    CONFERENCE_ANNOUNCEMENT_PLAYBACK_WINDOW, CallFeatureError, CallFeatureProvider, CallId,
    CallMetadata, CallTransition, CallTransitionProgress, ChannelAllocationError,
    ChannelAvailability, ChannelMetadataError, Codec, ConferenceAnnouncement,
    ConferenceAnnouncementOperation, ConferenceId, ConferenceTaskCancellation,
    ControlProviderError, DeviceId, DirectMediaCall, DriverEffect, Duration, EffectExecutionError,
    HandsetEffect, HashSet, Instant, LogLevel, MAX_RESTORE_ATTEMPTS, MediaAnchorReason,
    MediaEndpoint, MutexExt, NonNull, PARKING_NOTIFICATION_TIME, ParkingOperation,
    PartyUpdateError, PbxBackendError, PbxBridgeId, PbxCallId, PbxEffect, PbxServiceCapabilities,
    PhoneCallState, PhoneCommand, PhoneCommandAction, PickupOperation, ReceiveChannelPurpose,
    RecordingError, RecordingTarget, RedirectReasonCode, RedirectingUpdate, RemoteHangupPlan,
    RuntimeCallSignalDeliveryError, RuntimeCallSignalDeliveryResult, ast_log, audio_framing,
    c_string, cancel_no_answer_timer, channel_availability, configured_audio_processing,
    configured_audio_traffic_class, configured_dtmf_mode, direct_media_call,
    execute_backend_cleanup_effects, handset_effect_call_id, local_media_endpoint,
    native_audio_format, native_bridging, native_channel, pbx_audio_format, publish_ami_event,
    publish_line, redirected_call_update, remove_channel, replacement_anchor_plan,
    restore_attempts_exhausted, restore_redirecting_update, show_conference_list,
    start_announcement, validate_native_channel_metadata, validate_redirecting_update,
    with_channel,
};
use super::{
    AsteriskRecording, BridgeBackend, CString, CallServiceBackend, ChannelAllocationOwner,
    ChannelAllocationRequest, ChannelBackend, ConferenceDestinationOperation,
    ConferenceTaskStartError, ForwardingOperation, ForwardingRouteReason, IpAddr, IpAddressType,
    Ipv4Addr, LineBinding, LineInstance, MANAGER_CONTROL_DELIVERY_TIMEOUT, ManagementBackend,
    ManagementEvent, MediaBackend, MediaEndpointAddress, MultimediaReceiveDescriptor,
    MultimediaTransmitControl, MultimediaTransmitDescriptor, NORMAL_CLEARING, PbxVideoFormat,
    PickupOutcome, ProtocolVersion, RecordingCallback, RecordingDirection, RecordingProvider,
    RecordingSession, RecordingSessionControl, RecordingState, SupplementaryBackend,
    TransferCompletion, VoicemailOperation, allocate_channel, call_event,
    configured_video_traffic_class, native_pickup_result, prepare_channel_allocation_text, ptr,
};
use crate::media::encryption::LocalEncryptionCapabilities;
use crate::runtime::backend::PbxBackend as _;
use crate::runtime::controller::VideoPlan;
use sccp_protocol::SessionGeneration;

mod bridge_effects;
mod call_service;
mod channel;
mod handset;
mod management_effects;
mod media_effects;
mod recording;
mod supplementary;
pub use handset::{execute_handset_effect, send_handset_call_state};
pub use recording::AsteriskRecordingService;
pub(super) use recording::{
    AnchoredRecordingSession, ConfirmedRecordingAnchor, PendingRecordingAnchor,
};

impl ConferenceTaskCancellation for native_bridging::ConferenceApplicationCancellation {
    fn cancel(self) {
        native_bridging::ConferenceApplicationCancellation::cancel(self);
    }
}

pub async fn execute_effects(access: &Access, effects: Vec<DriverEffect>) {
    let _ = execute_effects_confirmed(access, effects).await;
}

pub async fn execute_effects_confirmed(
    access: &Access,
    effects: Vec<DriverEffect>,
) -> RuntimeCallSignalDeliveryResult {
    let backend = AsteriskBackend::new(access);
    for (index, effect) in effects.into_iter().enumerate() {
        let result = execute_one_effect(access, &backend, index, effect).await;
        if let Err(error) = result {
            let continue_after_failure = matches!(
                &error,
                EffectExecutionError::Backend {
                    effect,
                    ..
                } if matches!(
                    effect.as_ref(),
                    PbxEffect::Barge {
                        operation: BargeOperation::Leave { .. }
                    }
                        | PbxEffect::Bridge {
                            operation: BridgeOperation::Destroy { .. }
                        }
                        | PbxEffect::ConferenceAnnouncement { .. }
                )
            );
            if continue_after_failure {
                ast_log(
                    LogLevel::Warning,
                    &format!("non-blocking SCCP effect failed; continuing: {error}"),
                );
                continue;
            }
            handle_effect_error(access, &backend, error).await;
            return Err(RuntimeCallSignalDeliveryError);
        }
    }
    Ok(())
}

pub async fn execute_answer_call_transition(access: &Access, transition: CallTransition) -> bool {
    let pbx_id = transition.target_pbx_id;
    let committed = execute_call_transition(access, transition).await;
    if committed {
        cancel_no_answer_timer(access, pbx_id);
        access.enqueue_recording_eligibility(pbx_id);
    }
    committed
}

pub async fn execute_remote_hangup_plan(access: &Access, plan: RemoteHangupPlan) {
    let backend = AsteriskBackend::new(access);
    let mut failed = false;
    for (index, effect) in plan.outcome.effects.into_iter().enumerate() {
        if let Err(error) = execute_one_effect(access, &backend, index, effect).await {
            failed = true;
            ast_log(
                LogLevel::Warning,
                &format!("SCCP remote-hangup presentation failed: {error}"),
            );
        }
    }
    if failed
        && let Some(token) = plan.pending
        && let Some(effect) = access
            .shared
            .controller
            .complete_remote_hangup_token(token)
            .unwrap_or_else(|_| None)
    {
        execute_cleanup_effects(access, vec![effect]).await;
    }
}

pub async fn execute_call_transition(access: &Access, transition: CallTransition) -> bool {
    execute_call_transition_result(access, transition)
        .await
        .unwrap_or(false)
}

pub async fn execute_call_transition_result(
    access: &Access,
    transition: CallTransition,
) -> Result<bool, ControlProviderError> {
    let line = access
        .shared
        .controller
        .snapshot()
        .active_or_primary_call_by_pbx(transition.target_pbx_id)
        .map(|call| call.line);
    let backend = AsteriskBackend::new(access);
    let mut progress = CallTransitionProgress::default();
    for (index, effect) in transition.effects.iter().cloned().enumerate() {
        match execute_one_effect(access, &backend, index, effect.clone()).await {
            Ok(()) => {
                progress.record_success(&transition, &effect);
                let recorded = access
                    .shared
                    .controller
                    .record_call_transition_success(transition.id, &effect)
                    .unwrap_or_else(|_| false);
                if !recorded {
                    let compensation = access
                        .shared
                        .controller
                        .compensate_unrecorded_call_transition_effect(&transition, &effect)
                        .unwrap_or_else(|_| Default::default());
                    execute_cleanup_effects(access, compensation.effects).await;
                    if compensation.remove_target_channel {
                        remove_channel(access, transition.target_pbx_id);
                    }
                    return Ok(false);
                }
            }
            Err(error) => {
                let provider_error = match &error {
                    EffectExecutionError::Backend { .. } => ControlProviderError::Backend,
                    EffectExecutionError::Handset { .. } => ControlProviderError::HandsetDelivery,
                };
                ast_log(
                    LogLevel::Warning,
                    &format!("SCCP call transition failed: {error}"),
                );
                let remove_target_channel = transition.remove_target_channel_on_abort(&progress);
                let cleanup = access
                    .shared
                    .controller
                    .abort_call_transition(transition.id, &progress)
                    .unwrap_or_else(|_| Vec::new());
                execute_cleanup_effects(access, cleanup).await;
                if remove_target_channel {
                    remove_channel(access, transition.target_pbx_id);
                }
                return Err(provider_error);
            }
        }
    }
    let committed = access
        .shared
        .controller
        .commit_call_transition(transition.id)
        .unwrap_or_else(|_| false);
    if committed && let Some(line) = line {
        publish_line(access, &line);
    }
    Ok(committed)
}

pub async fn execute_cleanup_effects(access: &Access, effects: Vec<DriverEffect>) {
    let backend = AsteriskBackend::new(access);
    let errors = execute_backend_cleanup_effects(&backend, effects, |effect| {
        execute_handset_effect(access, effect)
    })
    .await;
    for error in errors {
        ast_log(
            LogLevel::Warning,
            &format!("SCCP cleanup effect failed: {error}"),
        );
    }
}

pub async fn execute_one_effect(
    access: &Access,
    backend: &AsteriskBackend<'_>,
    index: usize,
    effect: DriverEffect,
) -> Result<(), EffectExecutionError<AsteriskBackendError, String>> {
    let discard_stale_media_effect = match &effect {
        DriverEffect::Backend(
            PbxEffect::ConfigureMedia { call_id, .. }
            | PbxEffect::ConfigureMediaOnly { call_id, .. },
        ) => channel_availability(access, *call_id) == ChannelAvailability::Retiring,
        DriverEffect::Handset(
            HandsetEffect::BeginMedia { call_id, .. }
            | HandsetEffect::BeginAnswerMedia { call_id, .. }
            | HandsetEffect::BeginOutboundMedia { call_id, .. }
            | HandsetEffect::BeginOneWayMedia { call_id, .. }
            | HandsetEffect::BeginEarlyMedia { call_id, .. }
            | HandsetEffect::StartMedia { call_id, .. },
        ) => {
            let pbx_id = access.shared.controller.snapshot().call_pbx_id(*call_id);
            pbx_id.is_none_or(|pbx_id| {
                channel_availability(access, pbx_id) == ChannelAvailability::Retiring
            })
        }
        _ => false,
    };
    if discard_stale_media_effect {
        return Ok(());
    }
    match effect {
        DriverEffect::Backend(PbxEffect::PublishManagementEvent { event }) => {
            access
                .shared
                .ami_events
                .publish_confirmed(event.clone())
                .await
                .map_err(|error| EffectExecutionError::Backend {
                    index,
                    effect: Box::new(PbxEffect::PublishManagementEvent { event }),
                    error: AsteriskBackendError::Management(error),
                })?;
        }
        DriverEffect::Backend(PbxEffect::Transfer { operation }) => {
            access
                .shared
                .bridge_runtime
                .execute_async(
                    access,
                    super::bridge_owner::BridgeAction::Transfer(operation.clone()),
                )
                .await
                .map_err(|error| EffectExecutionError::Backend {
                    index,
                    effect: Box::new(PbxEffect::Transfer { operation }),
                    error,
                })?;
        }
        DriverEffect::Backend(PbxEffect::Bridge { operation }) => {
            access
                .shared
                .bridge_runtime
                .execute_async(
                    access,
                    super::bridge_owner::BridgeAction::Bridge(operation.clone()),
                )
                .await
                .map_err(|error| EffectExecutionError::Backend {
                    index,
                    effect: Box::new(PbxEffect::Bridge { operation }),
                    error,
                })?;
        }
        DriverEffect::Backend(PbxEffect::Barge { operation }) => {
            access
                .shared
                .bridge_runtime
                .execute_async(
                    access,
                    super::bridge_owner::BridgeAction::Barge(operation.clone()),
                )
                .await
                .map_err(|error| EffectExecutionError::Backend {
                    index,
                    effect: Box::new(PbxEffect::Barge { operation }),
                    error,
                })?;
        }
        DriverEffect::Backend(effect) => {
            let followup =
                backend
                    .execute(&effect)
                    .map_err(|error| EffectExecutionError::Backend {
                        index,
                        effect: Box::new(effect.clone()),
                        error,
                    })?;
            if let Some(effect) = followup {
                execute_handset_effect(access, effect.clone())
                    .await
                    .map_err(|error| EffectExecutionError::Handset {
                        index,
                        effect: Box::new(effect),
                        error,
                    })?;
            }
        }
        DriverEffect::Handset(effect) => {
            execute_handset_effect(access, effect.clone())
                .await
                .map_err(|error| EffectExecutionError::Handset {
                    index,
                    effect: Box::new(effect),
                    error,
                })?;
        }
    }
    Ok(())
}

pub fn handset_effects(effects: Vec<DriverEffect>) -> Vec<DriverEffect> {
    effects
        .into_iter()
        .filter(|effect| matches!(effect, DriverEffect::Handset(_)))
        .collect()
}

pub struct AsteriskBackend<'a> {
    pub access: &'a Access,
    pub persistence: AsteriskDatabase,
    pub hints: AsteriskHints,
    pub recordings: AsteriskRecordingService<'a>,
    pub call_features: AsteriskCallFeatures,
}

pub type ActiveConferenceAnnouncement =
    crate::runtime::media_ownership::ActiveConferenceAnnouncement<Access, DirectMediaCall>;

impl AnnouncementCall for DirectMediaCall {
    fn call_id(&self) -> PbxCallId {
        self.pbx_id
    }
}

type MediaAnchorLease = super::media_owner::AnchorLease;

pub(super) struct MediaAnchorMutation {
    _reservation: super::media_owner::MediaReservation,
}

impl MediaAnchorMutation {
    pub(super) async fn acquire(access: &Access, call_id: PbxCallId) -> Option<Self> {
        access
            .shared
            .media_runtime
            .reserve(&[call_id], None)
            .await
            .map(|reservation| Self {
                _reservation: reservation,
            })
    }

    fn try_conference(
        access: &Access,
        conference_id: ConferenceId,
        calls: &[PbxCallId],
    ) -> Option<Self> {
        access
            .shared
            .media_runtime
            .try_reserve(calls, Some(conference_id))
            .map(|reservation| Self {
                _reservation: reservation,
            })
    }

    async fn conference(access: &Access, conference_id: ConferenceId) -> Option<Self> {
        access
            .shared
            .media_runtime
            .reserve(&[], Some(conference_id))
            .await
            .map(|reservation| Self {
                _reservation: reservation,
            })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AsteriskBackendError {
    #[error("{operation} failed for PBX call {calls}")]
    Failed {
        operation: &'static str,
        calls: String,
    },
    #[error("PBX call {} is unavailable for {operation}", call_id.0)]
    CallUnavailable {
        operation: &'static str,
        call_id: PbxCallId,
    },
    #[error("PBX call {} metadata update failed: {source}", call_id.0)]
    ChannelMetadata {
        call_id: PbxCallId,
        source: ChannelMetadataError,
    },
    #[error("PBX call {} channel allocation failed: {source}", call_id.0)]
    ChannelAllocation {
        call_id: PbxCallId,
        #[source]
        source: ChannelAllocationError,
    },
    #[error("PBX call {} redirecting update failed: {source}", call_id.0)]
    PartyUpdate {
        call_id: PbxCallId,
        source: PartyUpdateError,
    },
    #[error("PBX call {} has invalid native text for {operation}: {source}", call_id.0)]
    NativeText {
        operation: &'static str,
        call_id: PbxCallId,
        source: crate::asterisk::boundary::NativeTextError,
    },
    #[error(
        "PBX call {} redirect failed ({cause}) and rollback diverged at {restores}",
        call_id.0,
        restores = .failed_restores.join(", ")
    )]
    RedirectRollback {
        call_id: PbxCallId,
        #[source]
        cause: Box<AsteriskBackendError>,
        failed_restores: Vec<&'static str>,
    },
    #[error("PBX bridge {} is unavailable for {operation}", bridge_id.0)]
    BridgeUnavailable {
        operation: &'static str,
        bridge_id: PbxBridgeId,
    },
    #[error("PBX bridge {} already exists", bridge_id.0)]
    BridgeConflict { bridge_id: PbxBridgeId },
    #[error(transparent)]
    Management(AmiEventError),
    #[error(transparent)]
    CallFeature(CallFeatureError),
}

impl AsteriskBackend<'_> {
    pub fn new(access: &Access) -> AsteriskBackend<'_> {
        AsteriskBackend {
            access,
            persistence: AsteriskDatabase::new(),
            hints: AsteriskHints::new(),
            recordings: AsteriskRecordingService { access },
            call_features: AsteriskCallFeatures::new(),
        }
    }

    pub fn typed_operation_result<E>(
        operation: &'static str,
        call_id: PbxCallId,
        result: Option<Result<(), E>>,
    ) -> Result<(), AsteriskBackendError> {
        match result {
            Some(Ok(())) => Ok(()),
            Some(Err(_)) | None => Err(AsteriskBackendError::Failed {
                operation,
                calls: call_id.0.to_string(),
            }),
        }
    }

    pub fn with_call_feature_channel<T>(
        &self,
        operation: &'static str,
        call_id: PbxCallId,
        execute: impl FnOnce(&AsteriskChannel<'_>) -> Result<T, AsteriskBackendError>,
    ) -> Result<T, AsteriskBackendError> {
        with_channel(self.access, call_id, |channel| {
            let channel = unsafe { AsteriskChannel::from_raw(channel.cast()) }
                .map_err(|_| AsteriskBackendError::CallUnavailable { operation, call_id })?;
            execute(&channel)
        })
        .unwrap_or(Err(AsteriskBackendError::CallUnavailable {
            operation,
            call_id,
        }))
    }

    pub fn rollback_redirect(
        &self,
        channel: &AsteriskChannel<'_>,
        call_id: PbxCallId,
        native_metadata: &CallMetadata,
        controller_metadata: &CallMetadata,
        redirecting: &RedirectingUpdate,
    ) -> Vec<&'static str> {
        let mut failures = Vec::new();
        if AsteriskPartyUpdates::new()
            .set_redirecting(channel, redirecting)
            .is_err()
        {
            failures.push("native redirecting metadata");
        }
        if AsteriskChannelMetadata::new()
            .apply(channel, native_metadata)
            .is_err()
        {
            failures.push("native channel metadata");
        }
        if !matches!(
            self.access
                .shared
                .controller
                .set_call_metadata(call_id, controller_metadata.clone())
                .unwrap_or_else(|_| Ok(false)),
            Ok(true)
        ) {
            failures.push("controller channel metadata");
        }
        failures
    }

    pub fn redirect_failure(
        &self,
        channel: &AsteriskChannel<'_>,
        call_id: PbxCallId,
        native_metadata: &CallMetadata,
        controller_metadata: &CallMetadata,
        redirecting: &RedirectingUpdate,
        cause: AsteriskBackendError,
    ) -> AsteriskBackendError {
        let failed_restores = self.rollback_redirect(
            channel,
            call_id,
            native_metadata,
            controller_metadata,
            redirecting,
        );
        if failed_restores.is_empty() {
            cause
        } else {
            AsteriskBackendError::RedirectRollback {
                call_id,
                cause: Box::new(cause),
                failed_restores,
            }
        }
    }

    pub fn redirect_and_route(
        &self,
        call_id: PbxCallId,
        context: &str,
        destination: &str,
        reason: RedirectReasonCode,
    ) -> Result<(), AsteriskBackendError> {
        let controller_metadata = self
            .access
            .shared
            .controller
            .snapshot()
            .call_metadata(call_id)
            .cloned()
            .ok_or(AsteriskBackendError::CallUnavailable {
                operation: "snapshot redirect metadata",
                call_id,
            })?;
        let context = c_string(context).map_err(|source| AsteriskBackendError::NativeText {
            operation: "redirect call context",
            call_id,
            source,
        })?;
        let destination_c =
            c_string(destination).map_err(|source| AsteriskBackendError::NativeText {
                operation: "redirect call destination",
                call_id,
                source,
            })?;
        with_channel(self.access, call_id, |channel| {
            let channel = unsafe { AsteriskChannel::from_raw(channel.cast()) }.map_err(|_| {
                AsteriskBackendError::CallUnavailable {
                    operation: "redirect call",
                    call_id,
                }
            })?;
            let mut updated_metadata = AsteriskChannelMetadata::new()
                .snapshot(&channel)
                .map_err(|source| AsteriskBackendError::ChannelMetadata { call_id, source })?;
            let parties = AsteriskPartyUpdates::new()
                .snapshot(&channel)
                .map_err(|source| AsteriskBackendError::PartyUpdate { call_id, source })?;
            let redirecting = redirected_call_update(&parties, destination, reason)
                .map_err(|source| AsteriskBackendError::PartyUpdate { call_id, source })?;
            let restore_redirecting = restore_redirecting_update(&parties);
            let native_metadata = updated_metadata.clone();
            updated_metadata.dnid = Some(destination.to_owned());
            updated_metadata.validate().map_err(|source| {
                AsteriskBackendError::ChannelMetadata {
                    call_id,
                    source: source.into(),
                }
            })?;
            validate_native_channel_metadata(&updated_metadata)
                .map_err(|source| AsteriskBackendError::ChannelMetadata { call_id, source })?;
            validate_native_channel_metadata(&native_metadata)
                .map_err(|source| AsteriskBackendError::ChannelMetadata { call_id, source })?;
            validate_redirecting_update(&redirecting)
                .map_err(|source| AsteriskBackendError::PartyUpdate { call_id, source })?;
            validate_redirecting_update(&restore_redirecting)
                .map_err(|source| AsteriskBackendError::PartyUpdate { call_id, source })?;
            if let Err(source) = AsteriskChannelMetadata::new().apply(&channel, &updated_metadata) {
                let cause = AsteriskBackendError::ChannelMetadata { call_id, source };
                return Err(self.redirect_failure(
                    &channel,
                    call_id,
                    &native_metadata,
                    &controller_metadata,
                    &restore_redirecting,
                    cause,
                ));
            }
            if let Err(source) = AsteriskPartyUpdates::new().set_redirecting(&channel, &redirecting)
            {
                let cause = AsteriskBackendError::PartyUpdate { call_id, source };
                return Err(self.redirect_failure(
                    &channel,
                    call_id,
                    &native_metadata,
                    &controller_metadata,
                    &restore_redirecting,
                    cause,
                ));
            }
            match self
                .access
                .shared
                .controller
                .set_call_metadata(call_id, updated_metadata.clone())
                .unwrap_or_else(|_| Ok(false))
            {
                Ok(true) => {}
                Ok(false) => {
                    return Err(self.redirect_failure(
                        &channel,
                        call_id,
                        &native_metadata,
                        &controller_metadata,
                        &restore_redirecting,
                        AsteriskBackendError::CallUnavailable {
                            operation: "commit redirect metadata",
                            call_id,
                        },
                    ));
                }
                Err(source) => {
                    return Err(self.redirect_failure(
                        &channel,
                        call_id,
                        &native_metadata,
                        &controller_metadata,
                        &restore_redirecting,
                        AsteriskBackendError::ChannelMetadata {
                            call_id,
                            source: source.into(),
                        },
                    ));
                }
            }
            let raw_channel = NonNull::new(channel.as_raw().cast()).ok_or_else(|| {
                self.redirect_failure(
                    &channel,
                    call_id,
                    &native_metadata,
                    &controller_metadata,
                    &restore_redirecting,
                    AsteriskBackendError::CallUnavailable {
                        operation: "start redirected routing",
                        call_id,
                    },
                )
            })?;
            if unsafe { native_channel::start_dialplan(raw_channel, &context, &destination_c) }
                .is_err()
            {
                return Err(self.redirect_failure(
                    &channel,
                    call_id,
                    &native_metadata,
                    &controller_metadata,
                    &restore_redirecting,
                    AsteriskBackendError::Failed {
                        operation: "start redirected routing",
                        calls: call_id.0.to_string(),
                    },
                ));
            }
            Ok(())
        })
        .unwrap_or(Err(AsteriskBackendError::CallUnavailable {
            operation: "redirect call",
            call_id,
        }))
    }
}

impl<'a> PbxServiceCapabilities for AsteriskBackend<'a> {
    type Persistence = AsteriskDatabase;
    type Hints = AsteriskHints;
    type Recordings = AsteriskRecordingService<'a>;

    fn persistence(&self) -> &Self::Persistence {
        &self.persistence
    }

    fn hints(&self) -> &Self::Hints {
        &self.hints
    }

    fn recordings(&self) -> &Self::Recordings {
        &self.recordings
    }
}

impl PbxBackendError for AsteriskBackend<'_> {
    type Error = AsteriskBackendError;
}

fn conference_announcement_tone(announcement: ConferenceAnnouncement) -> native_channel::TonePair {
    const FREQUENCY_HZ: u16 = 425;
    const VOLUME: u16 = 8192;

    let duration = match announcement {
        ConferenceAnnouncement::ParticipantMuted(_) => Duration::from_millis(500),
        ConferenceAnnouncement::Connected
        | ConferenceAnnouncement::ParticipantJoined(_)
        | ConferenceAnnouncement::ParticipantUnmuted(_)
        | ConferenceAnnouncement::ParticipantRemoved(_)
        | ConferenceAnnouncement::ModeratorDeparted(_) => Duration::from_millis(200),
    };

    native_channel::TonePair::new(FREQUENCY_HZ, None, duration, VOLUME)
}

fn stop_conference_tones(access: &Access, call_ids: &[PbxCallId]) {
    for call_id in call_ids {
        let _ = with_channel(access, *call_id, |channel| {
            if let Some(channel) = NonNull::new(channel) {
                unsafe { native_channel::stop_tone_pair(channel) };
            }
        });
    }
}

fn restore_conference_media(
    access: &Access,
    calls: &[DirectMediaCall],
    _mutation: &MediaAnchorMutation,
) -> Vec<PbxCallId> {
    calls
        .iter()
        .filter_map(|call| {
            if retarget_to_direct(access, call) {
                return None;
            }
            ast_log(
                LogLevel::Warning,
                &format!(
                    "unable to restore direct media after conference announcement for PBX call {}",
                    call.pbx_id
                ),
            );
            Some(call.pbx_id)
        })
        .collect()
}

struct AsteriskAnnouncementAdapter<'a> {
    access: &'a Access,
    tone: native_channel::TonePair,
}

impl AnnouncementAdapter<DirectMediaCall> for AsteriskAnnouncementAdapter<'_> {
    fn retarget_to_anchor(&mut self, call: &DirectMediaCall) -> bool {
        retarget_to_anchor(self.access, call)
    }

    fn retarget_to_direct(&mut self, call: &DirectMediaCall) -> bool {
        retarget_to_direct(self.access, call)
    }

    fn start_tone(&mut self, call_id: PbxCallId) -> bool {
        with_channel(self.access, call_id, |channel| {
            NonNull::new(channel).is_some_and(|channel| unsafe {
                native_channel::start_tone_pair(channel, self.tone).is_ok()
            })
        })
        .unwrap_or(false)
    }

    fn stop_tone(&mut self, call_id: PbxCallId) {
        stop_conference_tones(self.access, &[call_id]);
    }
}

fn schedule_conference_announcement_completion(
    access: &Access,
    _conference_id: ConferenceId,
    _generation: AnnouncementGeneration,
) -> super::media_owner::AnnouncementTimer {
    super::media_owner::AnnouncementTimer::new(access, CONFERENCE_ANNOUNCEMENT_PLAYBACK_WINDOW)
}

fn defer_conference_announcement_completion(
    access: &Access,
    conference_id: ConferenceId,
    generation: AnnouncementGeneration,
) {
    access.shared.media_runtime.defer(
        conference_id,
        generation,
        super::media_owner::AnnouncementTimer::new(access, Duration::from_millis(10)),
    );
}

fn finish_conference_announcement(
    access: &Access,
    conference_id: ConferenceId,
    mut active: ActiveConferenceAnnouncement,
    mutation: &MediaAnchorMutation,
) -> Vec<PbxCallId> {
    active.completion.take();
    stop_conference_tones(access, &active.call_ids);
    active.call_ids.clear();
    let mut seen = HashSet::new();
    let restore_calls = active
        .anchors
        .iter()
        .filter(|anchor| seen.insert(anchor.call_id))
        .filter_map(|anchor| anchor.restore_call())
        .collect::<Vec<_>>();
    let failures = restore_conference_media(access, &restore_calls, mutation);
    if failures.is_empty() {
        for anchor in &mut active.anchors {
            anchor.release();
        }
        return Vec::new();
    }
    let failures = failures.into_iter().collect::<HashSet<_>>();
    active.direct_calls = restore_calls
        .into_iter()
        .filter(|call| failures.contains(&call.pbx_id))
        .collect();
    for anchor in &mut active.anchors {
        if !failures.contains(&anchor.call_id) {
            anchor.release();
        }
    }
    active
        .anchors
        .retain(|anchor| failures.contains(&anchor.call_id));
    active.restore_attempts = active.restore_attempts.saturating_add(1);
    if restore_attempts_exhausted(active.restore_attempts) {
        for anchor in &mut active.anchors {
            anchor.release();
        }
        return failures.into_iter().collect();
    }
    active.completion = Some(schedule_conference_announcement_completion(
        access,
        conference_id,
        active.generation,
    ));
    mutation
        ._reservation
        .commit_announcement(conference_id, active);
    Vec::new()
}

fn terminate_conference_restore_failures(access: &Access, call_ids: Vec<PbxCallId>) {
    let backend = AsteriskBackend::new(access);
    for call_id in call_ids {
        if let Err(error) = backend.hangup(call_id) {
            ast_log(
                LogLevel::Warning,
                &format!(
                    "unable to terminate PBX call {call_id} after conference media restore exhaustion: {error}"
                ),
            );
        }
        remove_channel(access, call_id);
    }
}

pub fn play_conference_announcement(
    access: &Access,
    operation: &ConferenceAnnouncementOperation,
) -> Result<(), AsteriskBackendError> {
    let participant_call_ids = operation
        .targets
        .iter()
        .map(|target| target.call_id)
        .collect::<Vec<_>>();
    let Some(media_anchor_mutation) =
        MediaAnchorMutation::try_conference(access, operation.conference_id, &participant_call_ids)
    else {
        return Err(AsteriskBackendError::Failed {
            operation: "play conference announcement",
            calls: "media anchor transaction busy".into(),
        });
    };
    if operation.targets.is_empty() {
        return Err(AsteriskBackendError::Failed {
            operation: "play conference announcement",
            calls: "none".into(),
        });
    }
    let generation = access
        .shared
        .media_runtime
        .next_generation()
        .ok_or_else(|| AsteriskBackendError::Failed {
            operation: "play conference announcement",
            calls: "generation exhausted".into(),
        })?;

    let participant_call_ids = operation
        .targets
        .iter()
        .map(|target| target.call_id)
        .collect::<Vec<_>>();
    let previous = access
        .shared
        .media_runtime
        .take_announcement(operation.conference_id);
    let (inherited, previous_anchors) = previous.map_or_else(
        || (Vec::new(), Vec::new()),
        |mut previous| {
            previous.completion.take();
            stop_conference_tones(access, &previous.call_ids);
            (previous.direct_calls, previous.anchors)
        },
    );
    let participant_set = participant_call_ids.iter().copied().collect::<HashSet<_>>();
    let (retained, retired): (Vec<_>, Vec<_>) = inherited
        .into_iter()
        .partition(|call| participant_set.contains(&call.pbx_id));
    let registry = &access.shared.media_runtime;
    let inherited_announcement_calls = retained
        .into_iter()
        .filter(|call| {
            !registry.anchored_for_other_reason(call.pbx_id, MediaAnchorReason::Announcement)
        })
        .collect::<Vec<_>>();
    let retired = retired
        .into_iter()
        .filter(|call| {
            !registry.anchored_for_other_reason(call.pbx_id, MediaAnchorReason::Announcement)
        })
        .collect::<Vec<_>>();
    let retired_failures = restore_conference_media(access, &retired, &media_anchor_mutation)
        .into_iter()
        .collect::<HashSet<_>>();
    let retired_recovery_calls = retired
        .into_iter()
        .filter(|call| retired_failures.contains(&call.pbx_id))
        .collect::<Vec<_>>();
    let previous_anchor_ids = previous_anchors
        .iter()
        .map(|anchor| anchor.call_id)
        .collect::<Vec<_>>();
    let anchor_plan = replacement_anchor_plan(
        &participant_call_ids,
        &previous_anchor_ids,
        &retired_failures,
    );
    let (mut anchors, released_previous_anchors): (Vec<_>, Vec<_>) = previous_anchors
        .into_iter()
        .partition(|anchor| anchor_plan.retain_previous.contains(&anchor.call_id));
    for mut anchor in released_previous_anchors {
        anchor.release();
    }
    let inherited_ids = inherited_announcement_calls
        .iter()
        .map(|call| call.pbx_id)
        .collect::<HashSet<_>>();
    let to_retarget = participant_call_ids
        .iter()
        .filter(|call_id| !inherited_ids.contains(call_id))
        .filter_map(|call_id| {
            with_channel(access, *call_id, |channel| {
                direct_media_call(access, channel)
            })
            .flatten()
        })
        .collect::<Vec<_>>();
    for call_id in &anchor_plan.acquire {
        let restore = to_retarget
            .iter()
            .find(|call| call.pbx_id == *call_id)
            .cloned();
        let Some(anchor) = access.shared.media_runtime.acquire(
            &media_anchor_mutation._reservation,
            access,
            *call_id,
            MediaAnchorReason::Announcement,
            restore,
        ) else {
            let active = ActiveConferenceAnnouncement {
                generation,
                call_ids: Vec::new(),
                completion: None,
                anchors,
                direct_calls: retired_recovery_calls
                    .into_iter()
                    .chain(inherited_announcement_calls)
                    .collect(),
                restore_attempts: 0,
            };
            let terminal = finish_conference_announcement(
                access,
                operation.conference_id,
                active,
                &media_anchor_mutation,
            );
            terminate_conference_restore_failures(access, terminal);
            return Err(AsteriskBackendError::CallUnavailable {
                operation: "acquire announcement anchor",
                call_id: *call_id,
            });
        };
        anchors.push(anchor);
    }
    let tone = conference_announcement_tone(operation.announcement);
    let mut adapter = AsteriskAnnouncementAdapter { access, tone };
    if let Err(failure) = start_announcement(
        &mut adapter,
        &inherited_announcement_calls,
        &to_retarget,
        &participant_call_ids,
    ) {
        let failed = failure
            .compensation_failures
            .iter()
            .copied()
            .chain(retired_failures.iter().copied())
            .collect::<HashSet<_>>();
        if failed.is_empty() {
            for mut anchor in anchors {
                anchor.release();
            }
        } else {
            let retained_calls = retired_recovery_calls
                .into_iter()
                .chain(inherited_announcement_calls)
                .chain(to_retarget)
                .filter(|call| failed.contains(&call.pbx_id))
                .collect::<Vec<_>>();
            let (retained_anchors, released_anchors): (Vec<_>, Vec<_>) = anchors
                .into_iter()
                .partition(|anchor| failed.contains(&anchor.call_id));
            for mut anchor in released_anchors {
                anchor.release();
            }
            let active = ActiveConferenceAnnouncement {
                generation,
                call_ids: Vec::new(),
                completion: Some(schedule_conference_announcement_completion(
                    access,
                    operation.conference_id,
                    generation,
                )),
                anchors: retained_anchors,
                direct_calls: retained_calls,
                restore_attempts: 0,
            };
            media_anchor_mutation
                ._reservation
                .commit_announcement(operation.conference_id, active);
        }
        let operation = match failure.stage {
            AnnouncementFailureStage::Retarget => "anchor conference announcement media",
            AnnouncementFailureStage::Tone => "play conference announcement",
        };
        return Err(AsteriskBackendError::CallUnavailable {
            operation,
            call_id: failure.call_id,
        });
    }

    let direct_calls = retired_recovery_calls
        .into_iter()
        .chain(inherited_announcement_calls)
        .chain(to_retarget)
        .collect::<Vec<_>>();
    let completion =
        schedule_conference_announcement_completion(access, operation.conference_id, generation);
    media_anchor_mutation._reservation.commit_announcement(
        operation.conference_id,
        ActiveConferenceAnnouncement {
            generation,
            call_ids: participant_call_ids,
            completion: Some(completion),
            anchors,
            direct_calls,
            restore_attempts: 0,
        },
    );
    Ok(())
}

pub fn complete_conference_announcement(
    access: &Access,
    conference_id: ConferenceId,
    generation: AnnouncementGeneration,
) -> bool {
    let Some(mutation) = MediaAnchorMutation::try_conference(access, conference_id, &[]) else {
        return true;
    };
    if access.shared.media_runtime.generation(conference_id) != Some(generation) {
        return false;
    }
    if let Some(active) = access.shared.media_runtime.take_announcement(conference_id) {
        let terminal = finish_conference_announcement(access, conference_id, active, &mutation);
        terminate_conference_restore_failures(access, terminal);
    }
    false
}

pub fn cancel_conference_announcement(access: &Access, conference_id: ConferenceId) {
    let Some(mutation) = MediaAnchorMutation::try_conference(access, conference_id, &[]) else {
        if let Some(generation) = access.shared.media_runtime.generation(conference_id) {
            defer_conference_announcement_completion(access, conference_id, generation);
        }
        return;
    };
    if let Some(active) = access.shared.media_runtime.take_announcement(conference_id) {
        let terminal = finish_conference_announcement(access, conference_id, active, &mutation);
        terminate_conference_restore_failures(access, terminal);
    }
}

pub(super) fn reap_conference_tasks(shared: &super::Shared) {
    let cancellations = shared
        .conference_destination_tasks
        .lock_unpoisoned()
        .reap_finished();
    for cancellation in cancellations {
        ConferenceTaskCancellation::cancel(cancellation);
    }
}

pub async fn shutdown_conferences(access: &Access) {
    let plans = access
        .shared
        .controller
        .drain_conferences_for_shutdown()
        .unwrap_or_else(|_| Vec::new());
    let mut conference_ids = plans
        .iter()
        .map(|plan| plan.conference_id)
        .chain(access.shared.media_runtime.announcement_ids())
        .collect::<Vec<_>>();
    conference_ids.sort_unstable();
    conference_ids.dedup();

    for conference_id in conference_ids {
        let Some(mutation) = MediaAnchorMutation::conference(access, conference_id).await else {
            continue;
        };
        if let Some(mut active) = access.shared.media_runtime.take_announcement(conference_id) {
            active.restore_attempts = MAX_RESTORE_ATTEMPTS.saturating_sub(1);
            let terminal = finish_conference_announcement(access, conference_id, active, &mutation);
            terminate_conference_restore_failures(access, terminal);
        }
    }
    for plan in plans {
        execute_cleanup_effects(access, plan.effects).await;
        for call_id in plan.call_ids {
            remove_channel(access, call_id);
        }
    }

    let (destination_cancellations, mut destination_tasks) = access
        .shared
        .conference_destination_tasks
        .lock_unpoisoned()
        .begin_shutdown();
    for cancellation in destination_cancellations {
        ConferenceTaskCancellation::cancel(cancellation);
    }
    while let Some(result) = destination_tasks.join_next().await {
        if let Err(error) = result {
            ast_log(
                LogLevel::Warning,
                &format!("conference destination task failed during unload: {error}"),
            );
        }
    }

    if let Err(error) = access
        .shared
        .bridge_runtime
        .execute_async(access, super::bridge_owner::BridgeAction::Drain)
        .await
    {
        ast_log(
            LogLevel::Warning,
            &format!("unable to drain native bridges during unload: {error}"),
        );
    }

    // A conference callback may already have committed controller cleanup
    // when unload cancels its async adapter task. Releasing the remaining map
    // ownership is therefore the final exact-once boundary for channel refs
    // and call-scoped media anchors. Native channel lifetime remains owned by
    // Asterisk after this module reference is released.
    let remaining_calls = access
        .shared
        .channels
        .lock_unpoisoned()
        .keys()
        .copied()
        .collect::<Vec<_>>();
    for call_id in remaining_calls {
        remove_channel(access, call_id);
    }
}

pub async fn shutdown_remote_hangups(access: &Access) {
    let effects = access
        .shared
        .controller
        .drain_remote_hangups()
        .unwrap_or_else(|_| Vec::new());
    execute_cleanup_effects(access, effects).await;
}

pub async fn shutdown_one_way_microphones(access: &Access) {
    let effects = access
        .shared
        .controller
        .drain_one_way_microphones()
        .unwrap_or_else(|_| Vec::new());
    execute_cleanup_effects(access, effects).await;
}

pub async fn begin_handset_media(
    access: &Access,
    device_id: DeviceId,
    call_id: CallId,
    codec: Codec,
    state: PhoneCallState,
) -> Result<(), String> {
    admit_clear_audio_media(access, &device_id, call_id)?;
    let framing =
        audio_framing(access, &device_id, call_id, codec).map_err(|error| error.to_string())?;
    let source = receive_media_source(access, &device_id, call_id, codec)?;
    let dtmf_mode = configured_dtmf_mode(access, &device_id, call_id);
    let audio_processing = configured_audio_processing(access, &device_id, call_id);
    if state != PhoneCallState::Connected {
        access
            .phone
            .send_confirmed(PhoneCommand::new(
                device_id.clone(),
                PhoneCommandAction::StopRinging { call_id },
            ))
            .await
            .map_err(|error| error.to_string())?;
    }
    send_handset_call_state(access, device_id.clone(), call_id, state).await?;
    if state == PhoneCallState::Connected
        && let Some(info) = access
            .shared
            .controller
            .snapshot()
            .call_info(call_id)
            .cloned()
    {
        access
            .phone
            .send_confirmed(PhoneCommand::new(
                device_id.clone(),
                PhoneCommandAction::SetCallInfo { call_id, info },
            ))
            .await
            .map_err(|error| error.to_string())?;
    }
    access
        .phone
        .send_confirmed(PhoneCommand::new(
            device_id,
            PhoneCommandAction::OpenReceiveChannel {
                call_id,
                purpose: ReceiveChannelPurpose::Media,
                source: Some(source),
                codec,
                packet_ms: framing.packet_ms,
                max_frames_per_packet: framing.max_frames_per_packet,
                dtmf_mode,
                audio_processing,
            },
        ))
        .await
        .map_err(|error| error.to_string())
}

/// Begin a handset answer by presenting provisional answer state immediately
/// before opening receive media. Full Connected presentation remains gated on
/// the receive-channel acknowledgement.
pub async fn begin_answer_media(
    access: &Access,
    device_id: DeviceId,
    call_id: CallId,
    codec: Codec,
) -> Result<(), String> {
    admit_clear_audio_media(access, &device_id, call_id)?;
    let framing =
        audio_framing(access, &device_id, call_id, codec).map_err(|error| error.to_string())?;
    let source = receive_media_source(access, &device_id, call_id, codec)?;
    let dtmf_mode = configured_dtmf_mode(access, &device_id, call_id);
    let audio_processing = configured_audio_processing(access, &device_id, call_id);
    access
        .phone
        .send_confirmed(PhoneCommand::new(
            device_id.clone(),
            PhoneCommandAction::StopRinging { call_id },
        ))
        .await
        .map_err(|error| error.to_string())?;
    access
        .phone
        .send_confirmed(PhoneCommand::new(
            device_id,
            PhoneCommandAction::OpenReceiveChannel {
                call_id,
                purpose: ReceiveChannelPurpose::InboundAnswer,
                source: Some(source),
                codec,
                packet_ms: framing.packet_ms,
                max_frames_per_packet: framing.max_frames_per_packet,
                dtmf_mode,
                audio_processing,
            },
        ))
        .await
        .map_err(|error| error.to_string())
}

/// Open both sides of outbound early media without an acknowledgement boundary.
/// This is not an answer transition: the handset must already be in an outbound
/// Proceed or RingOut state, which remains active while media is opened.
pub async fn begin_outbound_media(
    access: &Access,
    device_id: DeviceId,
    call_id: CallId,
    codec: Codec,
) -> Result<(), String> {
    admit_clear_audio_media(access, &device_id, call_id)?;
    let framing =
        audio_framing(access, &device_id, call_id, codec).map_err(|error| error.to_string())?;
    let mut endpoint = receive_media_source(access, &device_id, call_id, codec)?;
    endpoint.packet_ms = framing.packet_ms;
    endpoint.max_frames_per_packet = framing.max_frames_per_packet;
    let dtmf_mode = configured_dtmf_mode(access, &device_id, call_id);
    let audio_processing = configured_audio_processing(access, &device_id, call_id);
    let traffic_class = configured_audio_traffic_class(access, &device_id)
        .ok_or_else(|| format!("invalid audio traffic class for {device_id}"))?;
    access
        .phone
        .send_confirmed(PhoneCommand::new(
            device_id.clone(),
            PhoneCommandAction::OpenOutboundMedia {
                call_id,
                source: Some(endpoint),
                endpoint,
                codec,
                packet_ms: framing.packet_ms,
                max_frames_per_packet: framing.max_frames_per_packet,
                dtmf_mode,
                audio_processing,
                traffic_class,
            },
        ))
        .await
        .map_err(|error| error.to_string())?;
    access
        .phone
        .send_confirmed(PhoneCommand::new(
            device_id,
            PhoneCommandAction::DisplayPrompt {
                call_id,
                timeout_seconds: 0,
                text: "Call Progress".into(),
            },
        ))
        .await
        .map_err(|error| error.to_string())
}

pub fn receive_media_source(
    access: &Access,
    device_id: &DeviceId,
    call_id: CallId,
    codec: Codec,
) -> Result<MediaEndpoint, String> {
    let pbx_id = access
        .shared
        .controller
        .snapshot()
        .call(call_id)
        .filter(|call| &call.device_id == device_id)
        .map(|call| call.pbx_id)
        .ok_or_else(|| format!("call {call_id:?} has no Asterisk channel"))?;
    local_media_endpoint(access, pbx_id, device_id, codec)
        .ok_or_else(|| format!("call {call_id:?} has no local media endpoint"))
}

/// Close every ownership layer after an asynchronous PBX operation failed.
///
/// Merely queueing an Asterisk hangup is insufficient when `ast_pbx_start`
/// failed before a PBX thread took ownership: the module's retained channel,
/// controller appearance, and handset SessionCall can otherwise survive as a
/// Down orphan. Queue the native hangup first, detach controller state into
/// explicit terminal handset effects, and finally release the module's native
/// reference. A later Asterisk hangup callback is intentionally idempotent.
pub async fn terminate_failed_pbx_call(
    access: &Access,
    backend: &AsteriskBackend<'_>,
    call_id: PbxCallId,
) {
    let _ = backend.hangup(call_id);
    let outcome = access
        .shared
        .controller
        .pbx_hangup_with_effects(call_id)
        .unwrap_or_else(|_| None);
    if let Some(outcome) = outcome {
        execute_cleanup_effects(access, outcome.effects).await;
    }
    remove_channel(access, call_id);
}

pub async fn handle_effect_error(
    access: &Access,
    backend: &AsteriskBackend<'_>,
    error: EffectExecutionError<AsteriskBackendError, String>,
) {
    ast_log(
        LogLevel::Warning,
        &format!("SCCP effect execution failed: {error}"),
    );
    let effect = match error {
        EffectExecutionError::Backend { effect, .. } => effect,
        EffectExecutionError::Handset { effect, .. } => {
            let video_cleanup = access
                .shared
                .controller
                .recover_optional_video_effect_failure(&effect)
                .unwrap_or_else(|_| None);
            if let Some(video_cleanup) = video_cleanup {
                execute_cleanup_effects(access, video_cleanup).await;
                return;
            }
            if let Some(call_id) = handset_effect_call_id(&effect) {
                let failure = access
                    .shared
                    .controller
                    .conference_participant_failed(call_id)
                    .unwrap_or_else(|_| None);
                if let Some(failure) = failure {
                    let conference_id = failure.conference_id;
                    let surviving = failure.surviving_session.clone();
                    execute_cleanup_effects(access, failure.effects).await;
                    for pbx_id in failure.call_ids {
                        remove_channel(access, pbx_id);
                    }
                    if let Some(session) = surviving {
                        if access
                            .config()
                            .conference_for_device(&session.device_id)
                            .is_some_and(|conference| conference.show_conference_list)
                        {
                            show_conference_list(
                                access,
                                session.device_id,
                                session.original_handset_call_id,
                            )
                            .await;
                        }
                    } else {
                        cancel_conference_announcement(access, conference_id);
                    }
                    return;
                }
                if matches!(
                    effect.as_ref(),
                    HandsetEffect::BeginMedia { .. }
                        | HandsetEffect::BeginAnswerMedia { .. }
                        | HandsetEffect::BeginOutboundMedia { .. }
                ) {
                    let cleanup = access
                        .shared
                        .controller
                        .abort_present_barge(call_id)
                        .unwrap_or_else(|_| Vec::new());
                    if !cleanup.is_empty() {
                        let barger_pbx = cleanup.iter().find_map(|effect| match effect {
                            DriverEffect::Backend(PbxEffect::Hangup { call_id }) => Some(*call_id),
                            _ => None,
                        });
                        execute_cleanup_effects(access, cleanup).await;
                        if let Some(pbx_id) = barger_pbx {
                            remove_channel(access, pbx_id);
                        }
                        return;
                    }
                }
                if let Some(pbx_id) = access.shared.controller.snapshot().call_pbx_id(call_id) {
                    terminate_failed_pbx_call(access, backend, pbx_id).await;
                }
            }
            return;
        }
    };
    match *effect {
        PbxEffect::CreateChannel {
            handset_call_id,
            call_id,
            binding,
            ..
        } => {
            let (barge, cleanup) = access
                .shared
                .controller
                .reject_native_call(handset_call_id)
                .unwrap_or_else(|_| (false, Vec::new()));
            remove_channel(access, call_id);
            if barge {
                execute_cleanup_effects(access, cleanup).await;
            } else {
                let _ = access
                    .phone
                    .send(PhoneCommand::new(
                        binding.device_id,
                        PhoneCommandAction::CloseCall {
                            call_id: handset_call_id,
                        },
                    ))
                    .await;
            }
        }
        PbxEffect::Barge {
            operation: BargeOperation::Join { barger_call_id, .. },
        } => {
            let cleanup = access
                .shared
                .controller
                .abort_native_barge(barger_call_id)
                .unwrap_or_else(|_| Vec::new());
            execute_cleanup_effects(access, cleanup).await;
            remove_channel(access, barger_call_id);
        }
        PbxEffect::Pickup { operation } => {
            let (pbx_id, device_id, handset_call_id) = match operation {
                PickupOperation::Group {
                    call_id,
                    device_id,
                    handset_call_id,
                    ..
                }
                | PickupOperation::Directed {
                    call_id,
                    device_id,
                    handset_call_id,
                    ..
                } => (call_id, device_id, handset_call_id),
            };
            let cleanup = access
                .shared
                .controller
                .hangup(handset_call_id)
                .unwrap_or_else(|_| Vec::new());
            execute_cleanup_effects(access, cleanup).await;
            remove_channel(access, pbx_id);
            let _ = access
                .phone
                .send(PhoneCommand::new(
                    device_id.clone(),
                    PhoneCommandAction::DisplayPrompt {
                        call_id: handset_call_id,
                        timeout_seconds: 4,
                        text: "No call available for pickup".into(),
                    },
                ))
                .await;
            let _ = access
                .phone
                .send(PhoneCommand::new(
                    device_id,
                    PhoneCommandAction::CloseCall {
                        call_id: handset_call_id,
                    },
                ))
                .await;
        }
        PbxEffect::Parking { operation } => {
            let update = access
                .shared
                .controller
                .fail_parking_operation(operation, Instant::now() + PARKING_NOTIFICATION_TIME)
                .unwrap_or_default();
            crate::asterisk::phone::execute_parking_update(access, update).await;
        }
        PbxEffect::StartRouting { call_id, .. }
        | PbxEffect::ConfigureMedia { call_id, .. }
        | PbxEffect::ConfigureMediaOnly { call_id, .. } => {
            terminate_failed_pbx_call(access, backend, call_id).await;
        }
        PbxEffect::StartConferenceDestination { operation } => {
            let handset = access
                .shared
                .controller
                .snapshot()
                .active_or_primary_call_by_pbx(operation.call_id)
                .map(|call| (call.device_id.clone(), call.sccp_id));
            if let Some((device_id, call_id)) = handset {
                let _ = access
                    .phone
                    .send(PhoneCommand::new(
                        device_id,
                        PhoneCommandAction::DisplayPrompt {
                            call_id,
                            timeout_seconds: 4,
                            text: "Conference dialing failed".into(),
                        },
                    ))
                    .await;
            }
            let _ = backend.hangup(operation.call_id);
        }
        _ => {}
    }
}

#[cfg(test)]
mod error_tests {
    use std::error::Error as _;

    use super::*;

    #[test]
    fn backend_error_display_and_rollback_source_chain_are_stable() {
        let cause = AsteriskBackendError::Failed {
            operation: "start routing",
            calls: "42".to_owned(),
        };
        let error = AsteriskBackendError::RedirectRollback {
            call_id: PbxCallId(42),
            cause: Box::new(cause),
            failed_restores: vec!["metadata", "redirecting"],
        };
        assert_eq!(
            error.to_string(),
            "PBX call 42 redirect failed (start routing failed for PBX call 42) and rollback diverged at metadata, redirecting"
        );
        assert_eq!(
            error.source().map(ToString::to_string).as_deref(),
            Some("start routing failed for PBX call 42")
        );
    }

    #[test]
    fn recording_error_is_transparent_for_management_formatting() {
        let error = AsteriskRecordingServiceError::Recording(RecordingError::StartFailed);
        assert_eq!(error.to_string(), "unable to start recording");
        assert_eq!(
            error.source().map(ToString::to_string).as_deref(),
            Some("unable to start recording")
        );
    }
}
