//! The only production owner of controller transitions.
//!
//! A dedicated thread services short typed transitions. Native callers retain
//! borrowed channel pointers and receive owned preparation/commit results;
//! asynchronous/native effects never execute on this thread. Typed commands are
//! boxed at admission so configuration payloads do not inflate every physical
//! mailbox slot, including slots preallocated for lifetime completions.

use std::sync::{Arc, RwLock, mpsc};
use std::time::{Duration, Instant};

use super::codec_mutation::CodecMutation;
use super::completion::{
    CompletionKey, CompletionLane, CompletionRequest, CompletionRoutes, current_resources,
    reserve_lane,
};
use super::*;
use crate::call::forwarding::{
    ForwardingCommit, ForwardingDigitOutcome, ForwardingEntry, ForwardingEntryId,
    ForwardingEntryTiming, ForwardingExpiryOutcome, ForwardingKind, ForwardingWriteOutcome,
};
use crate::call::forwarding::{
    ForwardingContext, ForwardingOperation, ForwardingRejection, NoAnswerTimer, NoAnswerTimerId,
};
use crate::call::shared_lines::SharedNoAnswerRoute;
use crate::media::formats::PbxAudioFormat;
use crate::runtime::mailbox::{
    AdmissionError, MailboxReceiver, MailboxSender, QueueSnapshot, RUNTIME_MAILBOX_CAPACITY,
    mailbox,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum ControllerRequestError {
    #[error(transparent)]
    Admission(#[from] AdmissionError),
    #[error("controller owner stopped before replying")]
    OwnerStopped,
}

#[derive(Clone)]
pub(crate) struct ControllerHandle {
    // Keep channel blocks compact even when a variant carries a full policy.
    commands: MailboxSender<Box<ControllerCommand>>,
    completion_diagnostics: MailboxSender<CompletionRequest>,
    snapshot: Arc<RwLock<Arc<ControllerSnapshot>>>,
    routing: Arc<RwLock<Arc<CompletionRoutes>>>,
}

pub(crate) struct ControllerOwner {
    controller: Controller,
    commands: MailboxReceiver<Box<ControllerCommand>>,
    completions: MailboxReceiver<CompletionRequest>,
    completion_sender: MailboxSender<CompletionRequest>,
    routes: CompletionRoutes,
    snapshot: Arc<RwLock<Arc<ControllerSnapshot>>>,
    routing: Arc<RwLock<Arc<CompletionRoutes>>>,
}

impl ControllerOwner {
    pub fn new(controller: Controller) -> Result<(ControllerHandle, Self), ControllerRequestError> {
        Self::with_capacity(controller, RUNTIME_MAILBOX_CAPACITY)
    }

    fn with_capacity(
        controller: Controller,
        capacity: usize,
    ) -> Result<(ControllerHandle, Self), ControllerRequestError> {
        let snapshot = Arc::new(RwLock::new(Arc::new(controller.snapshot())));
        let (commands, receiver) = mailbox(capacity);
        let (completion_sender, completions) = mailbox(capacity);
        let mut routes = CompletionRoutes::new();
        for key in current_resources(&controller) {
            if !routes.contains_key(&key) {
                routes.insert(key, reserve_lane(&completion_sender)?);
            }
        }
        let routing = Arc::new(RwLock::new(Arc::new(routes.clone())));
        Ok((
            ControllerHandle {
                commands,
                completion_diagnostics: completion_sender.clone(),
                snapshot: Arc::clone(&snapshot),
                routing: Arc::clone(&routing),
            },
            Self {
                controller,
                commands: receiver,
                completions,
                completion_sender,
                routes,
                snapshot,
                routing,
            },
        ))
    }

    fn publish(&mut self, reserved: Option<Arc<CompletionLane>>) {
        self.controller.retire_ended_call_records();
        let mut reserved = reserved;
        let resources = current_resources(&self.controller)
            .into_iter()
            .collect::<HashSet<_>>();
        self.routes.retain(|key, lane| {
            let retain = resources.contains(key);
            if !retain {
                lane.retire();
            }
            retain
        });
        for key in resources {
            if !self.routes.contains_key(&key) {
                let lane = reserved.take().expect("typed call and registration preparations reserve at most one new resource before mutation");
                self.routes.insert(key, lane);
            }
        }
        *self
            .routing
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::new(self.routes.clone());
        *self
            .snapshot
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Arc::new(self.controller.snapshot());
    }

    fn execute(&mut self, command: Box<ControllerCommand>) {
        let reserved = if command.creates_resource(&self.controller) {
            match reserve_lane(&self.completion_sender) {
                Ok(lane) => Some(lane),
                Err(error) => {
                    command.reject(error.into());
                    return;
                }
            }
        } else {
            None
        };
        (*command).apply(self, reserved);
    }

    pub async fn run(mut self) {
        let mut ordinary_open = true;
        let mut completed_since_command = 0;
        loop {
            if ordinary_open && completed_since_command >= 32 {
                completed_since_command = 0;
                if let Ok(command) = self.commands.try_recv() {
                    let (command, _permit) = command.into_parts();
                    self.execute(command);
                    continue;
                }
            }
            tokio::select! {
                biased;
                completion = self.completions.recv() => {
                    let Some(completion) = completion else { break; };
                    let (completion, permit) = completion.into_parts();
                    self.execute(completion.command);
                    completed_since_command += 1;
                    completion.lane.recycle(&self.completion_sender, permit);
                }
                command = self.commands.recv(), if ordinary_open => {
                    match command {
                        Some(command) => { let (command, _permit) = command.into_parts(); self.execute(command); }
                        None => {
                            ordinary_open = false;
                            for lane in self.routes.values() { lane.retire(); }
                            self.routes.clear();
                            *self.routing.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::new(CompletionRoutes::new());
                            self.completions.close();
                        }
                    }
                }
            }
        }
    }
}

impl Drop for ControllerOwner {
    fn drop(&mut self) {
        for lane in self.routes.values() {
            lane.retire();
        }
        self.routes.clear();
        *self
            .routing
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::new(CompletionRoutes::new());
    }
}

impl ControllerHandle {
    fn submit(&self, command: ControllerCommand) -> Result<(), ControllerRequestError> {
        let command = Box::new(command);
        if let Some(key) = command.completion_key(&self.snapshot()) {
            let routes = Arc::clone(
                &self
                    .routing
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
            );
            let lane = routes
                .get(&key)
                .or_else(|| routes.get(&CompletionKey::Configuration))
                .ok_or(ControllerRequestError::OwnerStopped)?;
            match lane.send(command) {
                Ok(()) => Ok(()),
                Err(command) => routes
                    .get(&CompletionKey::Configuration)
                    .ok_or(ControllerRequestError::OwnerStopped)?
                    .send(command)
                    .map_err(|_| ControllerRequestError::OwnerStopped),
            }
        } else {
            self.commands.try_send(command, None).map_err(Into::into)
        }
    }

    async fn submit_async(&self, command: ControllerCommand) -> Result<(), ControllerRequestError> {
        let command = Box::new(command);
        if let Some(key) = command.completion_key(&self.snapshot()) {
            let routes = Arc::clone(
                &self
                    .routing
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
            );
            let lane = routes
                .get(&key)
                .or_else(|| routes.get(&CompletionKey::Configuration))
                .ok_or(ControllerRequestError::OwnerStopped)?;
            match lane.send_async(command).await {
                Ok(()) => Ok(()),
                Err(command) => routes
                    .get(&CompletionKey::Configuration)
                    .ok_or(ControllerRequestError::OwnerStopped)?
                    .send_async(command)
                    .await
                    .map_err(|_| ControllerRequestError::OwnerStopped),
            }
        } else {
            self.commands.send(command, None).await.map_err(Into::into)
        }
    }

    pub fn snapshot(&self) -> Arc<ControllerSnapshot> {
        Arc::clone(
            &self
                .snapshot
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    pub fn close(&self) {
        self.commands.close();
    }
    pub fn diagnostics(&self) -> QueueSnapshot {
        self.commands.snapshot()
    }
    pub fn completion_diagnostics(&self) -> QueueSnapshot {
        self.completion_diagnostics.snapshot()
    }
}

pub(super) enum ControllerReply<T> {
    Sync(mpsc::SyncSender<Result<T, ControllerRequestError>>),
    Async(tokio::sync::oneshot::Sender<Result<T, ControllerRequestError>>),
}

impl<T> ControllerReply<T> {
    fn send(self, result: Result<T, ControllerRequestError>) {
        match self {
            Self::Sync(reply) => {
                let _ = reply.send(result);
            }
            Self::Async(reply) => {
                let _ = reply.send(result);
            }
        }
    }
}

macro_rules! controller_handle_method {
    ($variant:ident, $method:ident, ($($arg:ident: $public:ty => $owned:ty = $capture:expr),*), $result:ty) => {
        pub fn $method(&self, $($arg: $public),*) -> Result<$result, ControllerRequestError> {
            $(let $arg: $owned = $capture;)*
            let (reply, result) = mpsc::sync_channel(1);
            self.submit(ControllerCommand::$variant { $($arg,)* reply: ControllerReply::Sync(reply) })?;
            result.recv().map_err(|_| ControllerRequestError::OwnerStopped)?
        }
    };
    ($variant:ident, $method:ident, ($($arg:ident: $public:ty => $owned:ty = $capture:expr),*), $result:ty, $async_method:ident) => {
        #[cfg(test)]
        controller_handle_method!($variant, $method, ($($arg: $public => $owned = $capture),*), $result);
        pub async fn $async_method(&self, $($arg: $public),*) -> Result<$result, ControllerRequestError> {
            $(let $arg: $owned = $capture;)*
            let (reply, result) = tokio::sync::oneshot::channel();
            self.submit_async(ControllerCommand::$variant { $($arg,)* reply: ControllerReply::Async(reply) }).await?;
            result.await.map_err(|_| ControllerRequestError::OwnerStopped)?
        }
    };
}

macro_rules! controller_commands {
    ($( $variant:ident => $method:ident (
        $( $arg:ident: $public:ty => $owned:ty = $capture:expr => $pass:expr ),* $(,)?
    ) -> $result:ty $(, async $async_method:ident)?; )*) => {
        #[cfg_attr(
            all(test, feature = "development"),
            expect(dead_code, reason = "owner tests retain the complete command catalog without native producers")
        )]
        pub(super) enum ControllerCommand {
            $( $variant { $($arg: $owned,)* reply: ControllerReply<$result> }, )*
        }

        impl ControllerCommand {
            fn apply(self, owner: &mut ControllerOwner, reserved: Option<Arc<CompletionLane>>) {
                match self {
                    $( Self::$variant { $($arg,)* reply } => {
                        let result = owner.controller.$method($($pass),*);
                        owner.publish(reserved);
                        let _ = reply.send(Ok(result));
                    }, )*
                }
            }

            fn reject(self, error: ControllerRequestError) {
                match self { $( Self::$variant { reply, .. } => { let _ = reply.send(Err(error)); }, )* }
            }
        }

        #[cfg_attr(
            all(test, feature = "development"),
            expect(dead_code, reason = "native handle entrypoints are compiled but not all invoked by header-free owner tests")
        )]
        impl ControllerHandle {
            $( controller_handle_method!($variant, $method, ($($arg: $public => $owned = $capture),*), $result $(, $async_method)?); )*
        }
    };
}

controller_commands! {
    ActivateRemoteHangup => activate_remote_hangup(
        token: RemoteHangupToken => RemoteHangupToken = token => token,
        presentation: Duration => Duration = presentation => presentation,
        now: Instant => Instant = now => now
    ) -> bool, async activate_remote_hangup_async;
    RetainAudioEncryption => retain_audio_encryption(
        call_id: CallId => CallId = call_id => call_id,
        device_id: DeviceId => DeviceId = device_id => device_id,
        generation: SessionGeneration => SessionGeneration = generation => generation,
        admission: crate::media::encryption::AudioEncryptionAdmission => crate::media::encryption::AudioEncryptionAdmission = admission => admission
    ) -> Option<crate::media::encryption::AudioEncryptionAdmission>;
    ClaimParking => claim_parking(
        lot: String => String = lot => lot,
        slot: u32 => u32 = slot => slot,
        device: DeviceId => DeviceId = device => device,
        call: CallId => CallId = call => call,
        deadline: Instant => Instant = deadline => deadline
    ) -> bool;
    ReleaseParkingClaim => release_parking_claim(
        lot: String => String = lot => lot,
        slot: u32 => u32 = slot => slot,
        call: CallId => CallId = call => call
    ) -> bool;
    SetParkPeer => set_park_peer(
        pbx_id: PbxCallId => PbxCallId = pbx_id => pbx_id,
        unique_id: String => String = unique_id => unique_id
    ) -> ();
    FailParkingOperation => fail_parking_operation(
        operation: ParkingOperation => ParkingOperation = operation => operation,
        notification_deadline: Instant => Instant = notification_deadline => notification_deadline
    ) -> parking::ParkingUpdate;
    ApplyParkingEvent => apply_parking_event(
        event: crate::call::parking::ParkingEvent => crate::call::parking::ParkingEvent = event => event,
        retriever: Option<CallId> => Option<CallId> = retriever => retriever,
        notification_deadline: Instant => Instant = notification_deadline => notification_deadline
    ) -> parking::ParkingUpdate;
    ExpireParkingAttempts => expire_parking_attempts(
        now: Instant => Instant = now => now,
        notification_deadline: Instant => Instant = notification_deadline => notification_deadline
    ) -> parking::ParkingUpdate, async expire_parking_attempts_async;
    ReserveMobilityPrompt => reserve_mobility_prompt(
        slot: crate::call::mobility::MobilitySlot => crate::call::mobility::MobilitySlot = slot => slot
    ) -> Option<MobilityPrompt>;
    TakeMobilityPrompt => take_mobility_prompt(
        device: &DeviceId => DeviceId = device.clone() => &device,
        id: sccp_protocol::TransactionId => sccp_protocol::TransactionId = id => id
    ) -> Option<crate::call::mobility::MobilitySlot>;
    PrepareMobilityLogin => prepare_mobility_login(
        slot: crate::call::mobility::MobilitySlot => crate::call::mobility::MobilitySlot = slot => slot,
        line: crate::config::LineConfig => crate::config::LineConfig = line => line,
        instances: Vec<u32> => Vec<u32> = instances => instances
    ) -> Result<crate::call::mobility::MobilityPreparation, crate::call::mobility::MobilityRegistryError>;
    PrepareMobilityLogout => prepare_mobility_logout(slot: &crate::call::mobility::MobilitySlot => crate::call::mobility::MobilitySlot = slot.clone() => &slot) -> Result<crate::call::mobility::PreparedMobilityTransaction, crate::call::mobility::MobilityRegistryError>;
    CommitMobility => commit_mobility(transaction: &crate::call::mobility::PreparedMobilityTransaction => crate::call::mobility::PreparedMobilityTransaction = transaction.clone() => &transaction) -> Result<(), crate::call::mobility::MobilityRegistryError>;
    AbortMobility => abort_mobility(transaction: &crate::call::mobility::PreparedMobilityTransaction => crate::call::mobility::PreparedMobilityTransaction = transaction.clone() => &transaction) -> Result<(), crate::call::mobility::MobilityRegistryError>;
    ReconcileMobility => reconcile_mobility(config: crate::config::ModuleConfig => Box<crate::config::ModuleConfig> = Box::new(config) => *config) -> Result<mobility::MobilityReconciliation, crate::call::mobility::MobilityRegistryError>;

    AbortCallTransition => abort_call_transition(
        id: CallTransitionId => CallTransitionId = id => id,
        progress: &CallTransitionProgress => CallTransitionProgress = progress.clone() => &progress
    ) -> Vec<DriverEffect>;
    AbortNativeBarge => abort_native_barge(
        barger_call_id: PbxCallId => PbxCallId = barger_call_id => barger_call_id
    ) -> Vec<DriverEffect>;
    AbortPresentBarge => abort_present_barge(
        call_id: CallId => CallId = call_id => call_id
    ) -> Vec<DriverEffect>;
    AbortReservedConference => abort_reserved_conference(
        mutation: ConferenceMutationToken => ConferenceMutationToken = mutation => mutation,
        call_id: CallId => CallId = call_id => call_id,
        bridge_created: bool => bool = bridge_created => bridge_created,
        channel_created: bool => bool = channel_created => channel_created,
        original_needs_resume: bool => bool = original_needs_resume => original_needs_resume,
        restore_original_media: bool => bool = restore_original_media => restore_original_media
    ) -> Vec<DriverEffect>;
    AbortReservedConferenceInvite => abort_reserved_conference_invite(
        mutation: ConferenceMutationToken => ConferenceMutationToken = mutation => mutation,
        invite_call_id: CallId => CallId = invite_call_id => invite_call_id,
        invite_channel_created: bool => bool = invite_channel_created => invite_channel_created,
        moderator_needs_resume: bool => bool = moderator_needs_resume => moderator_needs_resume,
        restore_moderator_media: bool => bool = restore_moderator_media => restore_moderator_media
    ) -> Vec<DriverEffect>;
    AbortReservedConferenceMute => abort_reserved_conference_mute(
        mutation: ConferenceMutationToken => ConferenceMutationToken = mutation => mutation,
        conference_id: ConferenceId => ConferenceId = conference_id => conference_id,
        participant_id: ParticipantId => ParticipantId = participant_id => participant_id,
        muted: bool => bool = muted => muted
    ) -> ();
    AbortReservedConferenceRemoval => abort_reserved_conference_removal(
        mutation: ConferenceMutationToken => ConferenceMutationToken = mutation => mutation,
        conference_id: ConferenceId => ConferenceId = conference_id => conference_id,
        participant_id: ParticipantId => ParticipantId = participant_id => participant_id
    ) -> bool;
    AbortReservedConferenceRoleChange => abort_reserved_conference_role_change(
        mutation: ConferenceMutationToken => ConferenceMutationToken = mutation => mutation,
        conference_id: ConferenceId => ConferenceId = conference_id => conference_id,
        participant_id: ParticipantId => ParticipantId = participant_id => participant_id,
        moderator: bool => bool = moderator => moderator
    ) -> ();
    AbortReservedHold => abort_reserved_hold(
        mutation: ConferenceMutationToken => ConferenceMutationToken = mutation => mutation,
        conference_id: ConferenceId => ConferenceId = conference_id => conference_id,
        participant_id: ParticipantId => ParticipantId = participant_id => participant_id,
        held: bool => bool = held => held,
        completed_music: Vec<ParticipantId> => Vec<ParticipantId> = completed_music => completed_music,
        handset_attempted: bool => bool = handset_attempted => handset_attempted
    ) -> Vec<DriverEffect>;
    AbortReservedJoin => abort_reserved_join(
        mutation: ConferenceMutationToken => ConferenceMutationToken = mutation => mutation,
        call_id: CallId => CallId = call_id => call_id,
        bridge_created: bool => bool = bridge_created => bridge_created,
        resumed_call_ids: Vec<PbxCallId> => Vec<PbxCallId> = resumed_call_ids => resumed_call_ids
    ) -> Vec<DriverEffect>;
    AbortTransfer => abort_transfer(
        device_id: &DeviceId => DeviceId = device_id.clone() => &device_id,
        transaction_id: TransferId => TransferId = transaction_id => transaction_id,
        reason: TransferCancellationReason => TransferCancellationReason = reason => reason
    ) -> Result<TransferTerminalOutcome, TransferRejection>;
    AbortVoicemail => abort_voicemail(
        device_id: &DeviceId => DeviceId = device_id.clone() => &device_id,
        transaction_id: VoicemailTransactionId => VoicemailTransactionId = transaction_id => transaction_id
    ) -> Result<VoicemailTransaction, VoicemailRejection>;
    AcceptMediaReceive => accept_media_receive(
        device_id: DeviceId => DeviceId = device_id => device_id,
        call_id: CallId => CallId = call_id => call_id,
        endpoint: MediaEndpoint => MediaEndpoint = endpoint => endpoint
    ) -> (Vec<DriverEffect>, bool);
    AcceptMediaTransmission => accept_media_transmission(
        device_id: &DeviceId => DeviceId = device_id.clone() => &device_id,
        call_id: CallId => CallId = call_id => call_id,
        endpoint: MediaEndpoint => MediaEndpoint = endpoint => endpoint
    ) -> (Vec<DriverEffect>, bool);
    ApplyPartySnapshot => apply_party_snapshot(
        pbx_id: PbxCallId => PbxCallId = pbx_id => pbx_id,
        snapshot: crate::pbx::party::PartySnapshot => crate::pbx::party::PartySnapshot = snapshot => snapshot
    ) -> Vec<DriverEffect>;
    ApplyPickupIdentity => apply_pickup_identity(
        call_id: CallId => CallId = call_id => call_id,
        parties: crate::runtime::backend::PickupOutcome => crate::runtime::backend::PickupOutcome = parties => parties
    ) -> CallInfo;
    Barge => barge(
        call_id: CallId => CallId = call_id => call_id,
        binding: LineBinding => LineBinding = binding => binding,
        codec: Codec => Codec = codec => codec,
        mode: BargeMode => BargeMode = mode => mode
    ) -> Result<Vec<DriverEffect>, BargeRejection>;
    BeginActiveCallSwitchTransaction => begin_active_call_switch_transaction(
        device_id: &DeviceId => DeviceId = device_id.clone() => &device_id,
        call_id: CallId => CallId = call_id => call_id
    ) -> Result<CallTransition, CallSwitchRejection>;
    BeginAdditionalPhoneCallTransaction => begin_additional_phone_call_transaction(
        sccp_id: CallId => CallId = sccp_id => sccp_id,
        binding: LineBinding => LineBinding = binding => binding,
        codec: Codec => Codec = codec => codec,
        now: Instant => Instant = now => now
    ) -> Result<CallTransition, CallSwitchRejection>;
    BeginConferenceDestination => begin_conference_destination(
        request: ConferenceDestinationRequest => ConferenceDestinationRequest = request => request
    ) -> Result<Vec<DriverEffect>, ConferenceDestinationRejection>;
    BeginDirectedPickup => begin_directed_pickup(
        call_id: CallId => CallId = call_id => call_id,
        permitted: bool => bool = permitted => permitted,
        enabled: bool => bool = enabled => enabled,
        context: String => String = context => context,
        answer: bool => bool = answer => answer
    ) -> Result<(), PickupRejection>;
    BeginHotlineCallTransaction => begin_hotline_call_transaction(
        request: HotlineCallRequest => HotlineCallRequest = request => request
    ) -> Result<CallTransition, CallSwitchRejection>;
    BeginImmediateDivert => begin_immediate_divert(
        device_id: &DeviceId => DeviceId = device_id.clone() => &device_id,
        call_id: CallId => CallId = call_id => call_id,
        target: VoicemailTarget => VoicemailTarget = target => target
    ) -> Result<VoicemailPlan, VoicemailRejection>;
    BeginSelectedVoicemailTransfer => begin_selected_voicemail_transfer(
        device_id: &DeviceId => DeviceId = device_id.clone() => &device_id,
        target: VoicemailTarget => VoicemailTarget = target => target
    ) -> Result<VoicemailPlan, VoicemailRejection>;
    BeginVideoTransmitForDevice => begin_video_transmit_for_device(
        device_id: &DeviceId => DeviceId = device_id.clone() => &device_id,
        session_generation: SessionGeneration => SessionGeneration = session_generation => session_generation,
        call_id: CallId => CallId = call_id => call_id
    ) -> Vec<DriverEffect>;
    CancelCallWaitingTone => cancel_call_waiting_tone(
        waiting_call_id: CallId => CallId = waiting_call_id => waiting_call_id
    ) -> bool;
    CancelFailedInboundOffer => cancel_failed_inbound_offer(
        call_id: CallId => CallId = call_id => call_id,
        pbx_id: PbxCallId => PbxCallId = pbx_id => pbx_id
    ) -> bool;
    CancelInboundOffer => cancel_inbound_offer(
        call_id: CallId => CallId = call_id => call_id
    ) -> bool;
    CommitCallTransition => commit_call_transition(
        id: CallTransitionId => CallTransitionId = id => id
    ) -> bool;
    CommitReservedConference => commit_reserved_conference(
        mutation: ConferenceMutationToken => ConferenceMutationToken = mutation => mutation,
        call_id: CallId => CallId = call_id => call_id,
        conference_id: ConferenceId => ConferenceId = conference_id => conference_id
    ) -> (bool, Option<Vec<DriverEffect>>);
    CommitReservedConferenceInvite => commit_reserved_conference_invite(
        mutation: ConferenceMutationToken => ConferenceMutationToken = mutation => mutation,
        invite_call_id: CallId => CallId = invite_call_id => invite_call_id,
        conference_id: ConferenceId => ConferenceId = conference_id => conference_id,
        participant_id: ParticipantId => ParticipantId = participant_id => participant_id
    ) -> (bool, Option<Vec<DriverEffect>>);
    CommitReservedConferenceMute => commit_reserved_conference_mute(
        mutation: ConferenceMutationToken => ConferenceMutationToken = mutation => mutation,
        conference_id: ConferenceId => ConferenceId = conference_id => conference_id,
        participant_id: ParticipantId => ParticipantId = participant_id => participant_id,
        muted: bool => bool = muted => muted
    ) -> bool;
    CommitReservedConferenceRemoval => commit_reserved_conference_removal(
        mutation: ConferenceMutationToken => ConferenceMutationToken = mutation => mutation,
        conference_id: ConferenceId => ConferenceId = conference_id => conference_id,
        participant_id: ParticipantId => ParticipantId = participant_id => participant_id
    ) -> Option<Vec<DriverEffect>>;
    CommitReservedConferenceRoleChange => commit_reserved_conference_role_change(
        mutation: ConferenceMutationToken => ConferenceMutationToken = mutation => mutation,
        conference_id: ConferenceId => ConferenceId = conference_id => conference_id,
        participant_id: ParticipantId => ParticipantId = participant_id => participant_id,
        moderator: bool => bool = moderator => moderator
    ) -> bool;
    CommitReservedHold => commit_reserved_hold(
        mutation: ConferenceMutationToken => ConferenceMutationToken = mutation => mutation,
        conference_id: ConferenceId => ConferenceId = conference_id => conference_id,
        participant_id: ParticipantId => ParticipantId = participant_id => participant_id,
        held: bool => bool = held => held,
        completed_music: Vec<ParticipantId> => Vec<ParticipantId> = completed_music => completed_music,
        handset_attempted: bool => bool = handset_attempted => handset_attempted
    ) -> (bool, Vec<DriverEffect>);
    CompensateUnrecordedCallTransitionEffect => compensate_unrecorded_call_transition_effect(
        transition: &CallTransition => CallTransition = transition.clone() => &transition,
        effect: &DriverEffect => DriverEffect = effect.clone() => &effect
    ) -> CallTransitionCompensation;
    CompleteConferenceMutation => complete_conference_mutation(
        token: ConferenceMutationToken => ConferenceMutationToken = token => token
    ) -> bool;
    CompleteDeviceTransfer => complete_device_transfer(
        device_id: &DeviceId => DeviceId = device_id.clone() => &device_id,
        reported_call_id: Option<CallId> => Option<CallId> = reported_call_id => reported_call_id,
        trigger: TransferTrigger => TransferTrigger = trigger => trigger
    ) -> Result<TransferCompletionPlan, TransferRejection>;
    CompleteRemoteHangupToken => complete_remote_hangup_token(
        token: RemoteHangupToken => RemoteHangupToken = token => token
    ) -> Option<DriverEffect>;
    CompleteTransfer => complete_transfer(
        device_id: &DeviceId => DeviceId = device_id.clone() => &device_id,
        consultation_call_id: CallId => CallId = consultation_call_id => consultation_call_id,
        trigger: TransferTrigger => TransferTrigger = trigger => trigger
    ) -> Result<TransferCompletionPlan, TransferRejection>;
    CompleteVoicemailNative => complete_voicemail_native(
        device_id: &DeviceId => DeviceId = device_id.clone() => &device_id,
        transaction_id: VoicemailTransactionId => VoicemailTransactionId = transaction_id => transaction_id,
        pbx_call_id: PbxCallId => PbxCallId = pbx_call_id => pbx_call_id
    ) -> Result<VoicemailNativeOutcome, VoicemailRejection>;
    ConferenceDestinationFailed => conference_destination_failed(
        mutation: ConferenceMutationToken => ConferenceMutationToken = mutation => mutation,
        handset_call_id: CallId => CallId = handset_call_id => handset_call_id,
        held_calls: &[PbxCallId] => Vec<PbxCallId> = held_calls.to_vec() => &held_calls,
        completed_holds: &[PbxCallId] => Vec<PbxCallId> = completed_holds.to_vec() => &completed_holds
    ) -> Vec<DriverEffect>;
    ConferenceParticipantFailed => conference_participant_failed(
        call_id: CallId => CallId = call_id => call_id
    ) -> Option<ConferenceParticipantFailureOutcome>;
    DeferTransferAction => defer_transfer_action(
        device_id: &DeviceId => DeviceId = device_id.clone() => &device_id,
        transaction_id: TransferId => TransferId = transaction_id => transaction_id,
        action: DeferredTransferAction => DeferredTransferAction = action => action
    ) -> Result<(), TransferRejection>;
    Digit => digit(
        call_id: CallId => CallId = call_id => call_id,
        digit: Digit => Digit = digit => digit,
        now: Instant => Instant = now => now
    ) -> Vec<DriverEffect>;
    DrainConferencesForShutdown => drain_conferences_for_shutdown(

    ) -> Vec<ConferenceCleanupPlan>;
    DrainOneWayMicrophones => drain_one_way_microphones(

    ) -> Vec<DriverEffect>;
    DrainRemoteHangups => drain_remote_hangups(

    ) -> Vec<DriverEffect>;
    Enbloc => enbloc(
        call_id: CallId => CallId = call_id => call_id,
        number: String => String = number => number
    ) -> Vec<DriverEffect>;
    EndConferenceByModerator => end_conference_by_moderator(
        requester: &DeviceId => DeviceId = requester.clone() => &requester,
        conference_id: ConferenceId => ConferenceId = conference_id => conference_id
    ) -> Result<Vec<DriverEffect>, ConferenceEndRejection>;
    ExpireCallDeadlines => expire_call_deadlines(
        now: Instant => Instant = now => now
    ) -> (Vec<DriverEffect>, Vec<CallTransition>), async expire_call_deadlines_async;
    ExpireRemoteHangups => expire_remote_hangups(
        now: Instant => Instant = now => now
    ) -> Vec<DriverEffect>, async expire_remote_hangups_async;
    GroupPickup => group_pickup(
        call_id: CallId => CallId = call_id => call_id,
        permitted: bool => bool = permitted => permitted,
        answer: bool => bool = answer => answer
    ) -> Result<Vec<DriverEffect>, PickupRejection>;
    Hangup => hangup(
        call_id: CallId => CallId = call_id => call_id
    ) -> Vec<DriverEffect>;
    InstallVideoPlanForDevice => install_video_plan_for_device(
        device_id: &DeviceId => DeviceId = device_id.clone() => &device_id,
        call_id: CallId => CallId = call_id => call_id,
        plan: VideoPlan => VideoPlan = plan => plan,
        readiness: VideoPlanReadiness => VideoPlanReadiness = readiness => readiness
    ) -> bool;
    MediaRetargetCompensationEnqueueFailed => media_retarget_compensation_enqueue_failed(
        call_id: CallId => CallId = call_id => call_id,
        previous: MediaStreamState => MediaStreamState = previous => previous
    ) -> bool;
    MediaRetargetCompensationStarted => media_retarget_compensation_started(
        call_id: CallId => CallId = call_id => call_id
    ) -> Option<MediaStreamState>;
    MediaRetargetEnqueueFailed => media_retarget_enqueue_failed(
        call_id: CallId => CallId = call_id => call_id,
        previous: MediaEndpoint => MediaEndpoint = previous => previous
    ) -> bool;
    MediaRetargetStarted => media_retarget_started(
        call_id: CallId => CallId = call_id => call_id
    ) -> Option<MediaEndpoint>;
    OfferInboundCallWithPolicy => offer_inbound_call_with_policy(
        pbx_id: PbxCallId => PbxCallId = pbx_id => pbx_id,
        candidates: impl IntoIterator<Item = InboundAppearance> => Vec<InboundAppearance> = candidates.into_iter().collect() => candidates
    ) -> InboundCallDisposition;
    PbxAnswer => pbx_answer(
        pbx_id: PbxCallId => PbxCallId = pbx_id => pbx_id
    ) -> Vec<DriverEffect>;
    PbxHangupWithEffects => pbx_hangup_with_effects(
        pbx_id: PbxCallId => PbxCallId = pbx_id => pbx_id
    ) -> Option<PbxHangupOutcome>;
    PbxProceeding => pbx_proceeding(
        pbx_id: PbxCallId => PbxCallId = pbx_id => pbx_id
    ) -> Vec<DriverEffect>;
    PbxProgressWithMediaMode => pbx_progress_with_media_mode(
        pbx_id: PbxCallId => PbxCallId = pbx_id => pbx_id,
        early_media: bool => bool = early_media => early_media,
        outbound_media_mode: OutboundMediaMode => OutboundMediaMode = outbound_media_mode => outbound_media_mode
    ) -> Vec<DriverEffect>;
    PbxRinging => pbx_ringing(
        pbx_id: PbxCallId => PbxCallId = pbx_id => pbx_id
    ) -> Vec<DriverEffect>;
    PhoneAnswer => phone_answer(
        call_id: CallId => CallId = call_id => call_id
    ) -> Vec<DriverEffect>;
    PrepareConference => prepare_conference(
        request: ConferenceConsultationRequest => ConferenceConsultationRequest = request => request,
        media_policy: ConferenceMediaPolicy => ConferenceMediaPolicy = media_policy => media_policy
    ) -> Result<(ConferenceMutationToken, Vec<DriverEffect>), ConferenceRejection>;
    PrepareConferenceInvite => prepare_conference_invite(
        moderator_call_id: CallId => CallId = moderator_call_id => moderator_call_id,
        invite_call_id: CallId => CallId = invite_call_id => invite_call_id,
        binding: LineBinding => LineBinding = binding => binding,
        codec: Codec => Codec = codec => codec,
        now: Instant => Instant = now => now
    ) -> Result<(ConferenceMutationToken, Vec<DriverEffect>), ConferenceRejection>;
    PrepareConferenceMute => prepare_conference_mute(
        device_id: DeviceId => DeviceId = device_id => device_id,
        conference_id: ConferenceId => ConferenceId = conference_id => conference_id,
        participant_id: ParticipantId => ParticipantId = participant_id => participant_id,
        muted: bool => bool = muted => muted
    ) -> Result<(ConferenceMutationToken, Vec<DriverEffect>), ConferenceParticipantRejection>;
    PrepareConferenceRemoval => prepare_conference_removal(
        device_id: DeviceId => DeviceId = device_id => device_id,
        conference_id: ConferenceId => ConferenceId = conference_id => conference_id,
        participant_id: ParticipantId => ParticipantId = participant_id => participant_id
    ) -> Result<(ConferenceMutationToken, Vec<DriverEffect>), ConferenceParticipantRejection>;
    PrepareConferenceRoleChange => prepare_conference_role_change(
        device_id: DeviceId => DeviceId = device_id => device_id,
        conference_id: ConferenceId => ConferenceId = conference_id => conference_id,
        participant_id: ParticipantId => ParticipantId = participant_id => participant_id,
        moderator: bool => bool = moderator => moderator
    ) -> Result<(ConferenceMutationToken, Vec<DriverEffect>), ConferenceParticipantRejection>;
    PrepareConfirmConference => prepare_confirm_conference(
        call_id: CallId => CallId = call_id => call_id
    ) -> Result<(ConferenceMutationToken, Vec<DriverEffect>), ConferenceRejection>;
    PrepareConfirmConferenceInvite => prepare_confirm_conference_invite(
        call_id: CallId => CallId = call_id => call_id
    ) -> Result<(ConferenceMutationToken, Vec<DriverEffect>), ConferenceRejection>;
    PrepareDirectTransfer => prepare_direct_transfer(
        device_id: DeviceId => DeviceId = device_id => device_id
    ) -> (
        Result<TransferCompletionPlan, TransferRejection>,
        Option<CallId>,
    );
    PrepareDisconnect => prepare_disconnect(
        device_id: DeviceId => DeviceId = device_id => device_id,
        generation: SessionGeneration => SessionGeneration = generation => generation
    ) -> Option<(Vec<DriverEffect>, Vec<ConferenceSession>, Vec<ConferenceId>)>, async prepare_disconnect_async;
    PrepareHold => prepare_hold(
        call_id: CallId => CallId = call_id => call_id,
        held: bool => bool = held => held
    ) -> HoldPlan;
    PrepareJoinCalls => prepare_join_calls(
        device_id: DeviceId => DeviceId = device_id => device_id,
        call_id: CallId => CallId = call_id => call_id,
        permitted: bool => bool = permitted => permitted,
        media_policy: ConferenceMediaPolicy => ConferenceMediaPolicy = media_policy => media_policy
    ) -> Result<(ConferenceMutationToken, Vec<DriverEffect>), ConferenceRejection>;
    PrepareParking => prepare_parking(
        call_id: CallId => CallId = call_id => call_id,
        enabled: bool => bool = enabled => enabled,
        lot: Option<String> => Option<String> = lot => lot,
        deadline: Instant => Instant = deadline => deadline
    ) -> (
        Option<PbxCallId>,
        Result<Vec<DriverEffect>, ParkingRejection>,
    );
    PrepareParkingRetrieval => prepare_parking_retrieval(
        call_id: CallId => CallId = call_id => call_id,
        binding: LineBinding => LineBinding = binding => binding,
        codec: Codec => Codec = codec => codec,
        lot: String => String = lot => lot,
        slot: u32 => u32 = slot => slot,
        info: CallInfo => CallInfo = info => info,
        deadline: Instant => Instant = deadline => deadline
    ) -> (
        Option<PbxCallId>,
        Result<Vec<DriverEffect>, ParkingRejection>,
    );
    PreparePhoneCall => prepare_phone_call(
        call_id: CallId => CallId = call_id => call_id,
        binding: LineBinding => LineBinding = binding => binding,
        codec: Codec => Codec = codec => codec,
        now: Instant => Instant = now => now
    ) -> (Option<PbxCallId>, Vec<DriverEffect>);
    PreparePhoneHangup => prepare_phone_hangup(
        call_id: CallId => CallId = call_id => call_id,
        physical_on_hook: bool => bool = physical_on_hook => physical_on_hook
    ) -> (
        Option<ConferenceId>,
        Vec<DriverEffect>,
        Option<ConferenceSession>,
    );
    PrepareRegisterSession => prepare_register_session(
        session_generation: SessionGeneration => SessionGeneration = session_generation => session_generation,
        registration: DeviceRegistration => DeviceRegistration = registration => registration
    ) -> Option<(
        RegisterSessionOutcome,
        Vec<ConferenceId>,
        Vec<ConferenceSession>,
    )>, async prepare_register_session_async;
    PrepareRemoteHangup => prepare_remote_hangup(
        pbx_id: PbxCallId => PbxCallId = pbx_id => pbx_id,
        remote_hangup_tone: Option<Tone> => Option<Tone> = remote_hangup_tone => remote_hangup_tone,
        presentation: Duration => Duration = presentation => presentation,
        now: Instant => Instant = now => now
    ) -> (
        Option<ConferenceId>,
        Option<RemoteHangupPlan>,
        Option<ConferenceSession>,
    ), async prepare_remote_hangup_async;
    PrepareTransfer => prepare_transfer(
        request: TransferConsultationRequest => TransferConsultationRequest = request => request
    ) -> (
        Result<Vec<DriverEffect>, TransferRejection>,
        Option<TransferTransaction>,
    );
    RecordCallTransitionSuccess => record_call_transition_success(
        id: CallTransitionId => CallTransitionId = id => id,
        effect: &DriverEffect => DriverEffect = effect.clone() => &effect
    ) -> bool;
    RecoverOptionalVideoEffectFailure => recover_optional_video_effect_failure(
        effect: &HandsetEffect => HandsetEffect = effect.clone() => &effect
    ) -> Option<Vec<DriverEffect>>;
    RejectNativeCall => reject_native_call(
        handset_call_id: CallId => CallId = handset_call_id => handset_call_id
    ) -> (bool, Vec<DriverEffect>);
    ReloadPolicy => reload_policy(
        next: crate::config::ModuleConfig => Box<crate::config::ModuleConfig> = Box::new(next) => *next,
        feature_states: HashMap<DeviceId, DeviceFeatureState> => HashMap<DeviceId, DeviceFeatureState> = feature_states => feature_states,
        registered_after: HashSet<DeviceId> => HashSet<DeviceId> = registered_after => registered_after
    ) -> (
        Vec<DeviceId>,
        std::collections::BTreeMap<DeviceId, DeviceFeatureState>,
    );
    ScheduleReadyAutoAnswers => schedule_ready_auto_answers(
        pbx_id: PbxCallId => PbxCallId = pbx_id => pbx_id,
        auto_answer: AutoAnswerPolicy => AutoAnswerPolicy = auto_answer => auto_answer,
        now: Instant => Instant = now => now
    ) -> (
        Option<Result<usize, AutoAnswerScheduleRejection>>,
        Vec<CallTransition>,
    );
    SeedInboundIdentity => seed_inbound_identity(
        pbx_id: PbxCallId => PbxCallId = pbx_id => pbx_id,
        snapshot: crate::pbx::party::PartySnapshot => crate::pbx::party::PartySnapshot = snapshot => snapshot
    ) -> Vec<DriverEffect>;
    SetAutoAnswerRequest => set_auto_answer_request(
        pbx_id: PbxCallId => PbxCallId = pbx_id => pbx_id,
        request: AutoAnswerRequest => AutoAnswerRequest = request => request
    ) -> bool;
    SetCallMetadata => set_call_metadata(
        pbx_id: PbxCallId => PbxCallId = pbx_id => pbx_id,
        metadata: CallMetadata => CallMetadata = metadata => metadata
    ) -> Result<bool, MetadataError>;
    SetCallPrivacy => set_call_privacy(
        call_id: CallId => CallId = call_id => call_id,
        enabled: bool => bool = enabled => enabled
    ) -> bool;
    SetCalledParty => set_called_party(
        pbx_id: PbxCallId => PbxCallId = pbx_id => pbx_id,
        name: Option<String> => Option<String> = name => name,
        number: String => String = number => number
    ) -> Vec<DriverEffect>;
    SetVideoAudioOnlyForDevice => set_video_audio_only_for_device(
        device_id: &DeviceId => DeviceId = device_id.clone() => &device_id,
        session_generation: SessionGeneration => SessionGeneration = session_generation => session_generation,
        call_id: CallId => CallId = call_id => call_id,
        reason: VideoFallbackReason => VideoFallbackReason = reason => reason
    ) -> bool;
    SpeedDial => speed_dial(
        call_id: CallId => CallId = call_id => call_id,
        number: String => String = number => number,
        await_further_digits: bool => bool = await_further_digits => await_further_digits,
        now: Instant => Instant = now => now
    ) -> Vec<DriverEffect>;
    StartCallWaitingTone => start_call_waiting_tone(
        waiting_call_id: CallId => CallId = waiting_call_id => waiting_call_id,
        tone: Option<Tone> => Option<Tone> = tone => tone,
        interval: Duration => Duration = interval => interval,
        now: Instant => Instant = now => now
    ) -> Vec<DriverEffect>;
    Steal => steal(
        call_id: CallId => CallId = call_id => call_id
    ) -> Vec<DriverEffect>;
    ToggleCallPrivacy => toggle_call_privacy(
        call_id: CallId => CallId = call_id => call_id
    ) -> Option<(CallSnapshot, bool, bool)>;
    ToggleCallSelected => toggle_call_selected(
        device: &DeviceId => DeviceId = device.clone() => &device,
        call_id: CallId => CallId = call_id => call_id
    ) -> Option<bool>;
    TransferSetupCompleted => transfer_setup_completed(
        device_id: &DeviceId => DeviceId = device_id.clone() => &device_id,
        transaction_id: TransferId => TransferId = transaction_id => transaction_id,
        milestone: TransferSetupMilestone => TransferSetupMilestone = milestone => milestone
    ) -> Result<(), TransferRejection>;
    TransferSucceeded => transfer_succeeded(
        device_id: &DeviceId => DeviceId = device_id.clone() => &device_id,
        transaction_id: TransferId => TransferId = transaction_id => transaction_id
    ) -> Option<TransferTerminalOutcome>;
    UpdateCapabilities => update_capabilities(
        device: &DeviceId => DeviceId = device.clone() => &device,
        session_generation: SessionGeneration => SessionGeneration = session_generation => session_generation,
        capabilities: StationMediaCapabilities => StationMediaCapabilities = capabilities => capabilities
    ) -> bool;
    VideoFallbackForDevice => video_fallback_for_device(
        device_id: &DeviceId => DeviceId = device_id.clone() => &device_id,
        session_generation: SessionGeneration => SessionGeneration = session_generation => session_generation,
        call_id: CallId => CallId = call_id => call_id,
        reason: VideoFallbackReason => VideoFallbackReason = reason => reason
    ) -> VideoFallbackOutcome;
    VideoModeForDevice => video_mode_for_device(
        device_id: &DeviceId => DeviceId = device_id.clone() => &device_id,
        call_id: CallId => CallId = call_id => call_id
    ) -> Vec<DriverEffect>;
    VideoReceiveOpenedForDevice => video_receive_opened_for_device(
        device_id: &DeviceId => DeviceId = device_id.clone() => &device_id,
        session_generation: SessionGeneration => SessionGeneration = session_generation => session_generation,
        call_id: CallId => CallId = call_id => call_id,
        codec: Codec => Codec = codec => codec,
        endpoint: MediaEndpointAddress => MediaEndpointAddress = endpoint => endpoint
    ) -> bool;
    VideoTransmitOpenedForDevice => video_transmit_opened_for_device(
        device_id: &DeviceId => DeviceId = device_id.clone() => &device_id,
        session_generation: SessionGeneration => SessionGeneration = session_generation => session_generation,
        call_id: CallId => CallId = call_id => call_id,
        codec: Codec => Codec = codec => codec,
        endpoint: MediaEndpointAddress => MediaEndpointAddress = endpoint => endpoint,
        passthrough_party_id: PassthroughPartyId => PassthroughPartyId = passthrough_party_id => passthrough_party_id
    ) -> bool;
    CommitDeviceFeatures => commit_device_features(
        device_id: &DeviceId => DeviceId = device_id.clone() => &device_id,
        expected: Option<DeviceFeatureState> => Option<DeviceFeatureState> = expected => expected,
        next: DeviceFeatureState => DeviceFeatureState = next => next
    ) -> bool;
    RetireCallRuntime => retire_call_runtime(
        pbx_id: PbxCallId => PbxCallId = pbx_id => pbx_id
    ) -> ();
    RegisterForwardedCall => register_forwarded_call(
        operation: ForwardingOperation => ForwardingOperation = operation => operation
    ) -> bool;
    SetAssignedChannelId => set_assigned_channel_id(
        pbx_id: PbxCallId => PbxCallId = pbx_id => pbx_id,
        assigned: Option<String> => Option<String> = assigned => assigned
    ) -> bool;
    SetAudioPacketMs => set_audio_packet_ms(
        pbx_id: PbxCallId => PbxCallId = pbx_id => pbx_id,
        packet_ms: u32 => u32 = packet_ms => packet_ms
    ) -> bool;
    SetNoAnswerPlan => set_no_answer_plan(
        pbx_id: PbxCallId => PbxCallId = pbx_id => pbx_id,
        route: SharedNoAnswerRoute => SharedNoAnswerRoute = route => route
    ) -> bool;
    TakeNoAnswerPlan => take_no_answer_plan(
        pbx_id: PbxCallId => PbxCallId = pbx_id => pbx_id
    ) -> Option<SharedNoAnswerRoute>;
    ScheduleNoAnswerTimer => schedule_no_answer_timer(
        pbx_id: PbxCallId => PbxCallId = pbx_id => pbx_id,
        deadline: Instant => Instant = deadline => deadline,
        context: ForwardingContext => ForwardingContext = context => context,
        destination: ForwardingDestination => ForwardingDestination = destination => destination
    ) -> Result<NoAnswerTimer, ForwardingRejection>;
    ClaimNoAnswerRoutes => claim_no_answer_routes(
        now: Instant => Instant = now => now
    ) -> Vec<(NoAnswerTimer, Option<String>)>, async claim_no_answer_routes_async;
    FinishNoAnswerRoute => finish_no_answer_route(
        pbx_id: PbxCallId => PbxCallId = pbx_id => pbx_id,
        timer_id: NoAnswerTimerId => NoAnswerTimerId = timer_id => timer_id,
        succeeded: bool => bool = succeeded => succeeded
    ) -> Vec<DriverEffect>;
    CancelNoAnswerTimer => cancel_no_answer_timer(
        pbx_id: PbxCallId => PbxCallId = pbx_id => pbx_id
    ) -> bool;
    PrepareDeviceFeatures => prepare_device_features(
        device_id: &DeviceId => DeviceId = device_id.clone() => &device_id,
        defaults: DeviceFeatureState => DeviceFeatureState = defaults => defaults,
        mutation: crate::state::features::FeatureMutation => crate::state::features::FeatureMutation = mutation => mutation
    ) -> crate::state::features::DeviceFeaturePlan;
    BeginForwardingEntry => begin_forwarding_entry(
        device_id: DeviceId => DeviceId = device_id => device_id,
        line_instance: u32 => u32 = line_instance => line_instance,
        call_id: CallId => CallId = call_id => call_id,
        kind: ForwardingKind => ForwardingKind = kind => kind,
        dial_terminator: Digit => Digit = dial_terminator => dial_terminator,
        timing: ForwardingEntryTiming => ForwardingEntryTiming = timing => timing
    ) -> Result<ForwardingEntry, ForwardingRejection>;
    ExpireForwardingEntries => expire_forwarding_entries(
        now: Instant => Instant = now => now
    ) -> Vec<ForwardingExpiryOutcome>, async expire_forwarding_entries_async;
    SettleForwardingCollection => settle_forwarding_collection(
        device_id: &DeviceId => DeviceId = device_id.clone() => &device_id,
        entry_id: ForwardingEntryId => ForwardingEntryId = entry_id => entry_id,
        outcome: ForwardingWriteOutcome => ForwardingWriteOutcome = outcome => outcome
    ) -> Result<ForwardingWriteOutcome, ForwardingRejection>;
    SettleForwardingTerminal => settle_forwarding_terminal(
        device_id: &DeviceId => DeviceId = device_id.clone() => &device_id,
        entry_id: ForwardingEntryId => ForwardingEntryId = entry_id => entry_id,
        outcome: ForwardingWriteOutcome => ForwardingWriteOutcome = outcome => outcome
    ) -> Result<ForwardingWriteOutcome, ForwardingRejection>;
    CancelForwardingForCall => cancel_forwarding_for_call(
        device_id: &DeviceId => DeviceId = device_id.clone() => &device_id,
        call_id: CallId => CallId = call_id => call_id
    ) -> bool;
    InputForwardingDigit => input_forwarding_digit(
        device_id: &DeviceId => DeviceId = device_id.clone() => &device_id,
        call_id: CallId => CallId = call_id => call_id,
        digit: Digit => Digit = digit => digit,
        now: Instant => Instant = now => now
    ) -> Option<Result<ForwardingDigitOutcome, ForwardingRejection>>;
    BackspaceForwarding => backspace_forwarding(
        device_id: &DeviceId => DeviceId = device_id.clone() => &device_id,
        call_id: CallId => CallId = call_id => call_id,
        now: Instant => Instant = now => now
    ) -> bool;
    ReplaceForwardingDigits => replace_forwarding_digits(
        device_id: &DeviceId => DeviceId = device_id.clone() => &device_id,
        call_id: CallId => CallId = call_id => call_id,
        digits: &str => String = digits.to_owned() => &digits,
        now: Instant => Instant = now => now
    ) -> Option<Result<(), ForwardingRejection>>;
    BeginForwardingCommit => begin_forwarding_commit(
        device_id: &DeviceId => DeviceId = device_id.clone() => &device_id,
        call_id: CallId => CallId = call_id => call_id
    ) -> Option<Result<ForwardingCommit, ForwardingRejection>>;
    FinishForwardingCommit => finish_forwarding_commit(
        device_id: &DeviceId => DeviceId = device_id.clone() => &device_id,
        entry_id: ForwardingEntryId => ForwardingEntryId = entry_id => entry_id,
        succeeded: bool => bool = succeeded => succeeded
    ) -> bool;
    PreparePreDialCodec => prepare_pre_dial_codec(
        pbx_id: PbxCallId => PbxCallId = pbx_id => pbx_id,
        selected: Codec => Codec = selected => selected,
        preferences: Option<Vec<PbxAudioFormat>> => Option<Vec<PbxAudioFormat>> = preferences => preferences
    ) -> Result<CodecMutation, CodecPreferenceRejection>;
    PrepareHeldCodec => prepare_held_codec(
        pbx_id: PbxCallId => PbxCallId = pbx_id => pbx_id,
        call_id: CallId => CallId = call_id => call_id,
        selected: Codec => Codec = selected => selected
    ) -> Result<CodecMutation, CodecPreferenceRejection>;
    CommitCodecMutation => commit_codec_mutation(token: CodecMutation => CodecMutation = token => token) -> bool;
    AbortCodecMutation => abort_codec_mutation(token: CodecMutation => CodecMutation = token => token) -> ();

    CommitRegisteredFeatures => commit_registered_features(
        device_id: &DeviceId => DeviceId = device_id.clone() => &device_id,
        generation: SessionGeneration => SessionGeneration = generation => generation,
        features: DeviceFeatureState => DeviceFeatureState = features => features
    ) -> bool;

}

#[cfg(test)]
#[path = "ownership_tests.rs"]
mod tests;

#[path = "command_admission.rs"]
mod command_admission;
