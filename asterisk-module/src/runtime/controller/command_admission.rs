//! Admission classification for typed controller operations.

use super::*;

impl ControllerCommand {
    pub(super) fn creates_resource(&self, controller: &Controller) -> bool {
        match self {
            Self::Barge { .. } => true,
            Self::BeginAdditionalPhoneCallTransaction { .. } => true,
            Self::BeginHotlineCallTransaction { .. } => true,
            Self::OfferInboundCallWithPolicy { .. } => true,
            Self::PrepareConference { .. } => true,
            Self::PrepareConferenceInvite { .. } => true,
            Self::PrepareParkingRetrieval { .. } => true,
            Self::PreparePhoneCall { .. } => true,
            Self::PrepareRegisterSession { registration, .. } => {
                !controller.devices.contains_key(&registration.id)
            }
            Self::PrepareTransfer { .. } => true,
            Self::RegisterForwardedCall { .. } => true,
            _ => false,
        }
    }

    pub(super) fn completion_key(&self, snapshot: &ControllerSnapshot) -> Option<CompletionKey> {
        let call = |call_id| {
            snapshot
                .call_pbx_id(call_id)
                .map(CompletionKey::Call)
                .unwrap_or(CompletionKey::Configuration)
        };
        Some(match self {
            Self::RetainAudioEncryption { device_id, .. } => {
                CompletionKey::Device(device_id.clone())
            }
            Self::ReleaseParkingClaim { .. } => CompletionKey::Configuration,
            Self::SetParkPeer { pbx_id, .. } => CompletionKey::Call(*pbx_id),
            Self::FailParkingOperation { .. } => CompletionKey::Configuration,
            Self::ApplyParkingEvent { .. } => CompletionKey::Configuration,
            Self::ExpireParkingAttempts { .. } => CompletionKey::Configuration,
            Self::TakeMobilityPrompt { device, .. } => CompletionKey::Device(device.clone()),
            Self::CommitMobility { .. } => CompletionKey::Configuration,
            Self::AbortMobility { .. } => CompletionKey::Configuration,
            Self::ReconcileMobility { .. } => CompletionKey::Configuration,
            Self::AbortCallTransition { .. } => CompletionKey::Configuration,
            Self::AbortNativeBarge { barger_call_id, .. } => CompletionKey::Call(*barger_call_id),
            Self::AbortPresentBarge { call_id, .. } => call(*call_id),
            Self::AbortReservedConference { call_id, .. } => call(*call_id),
            Self::AbortReservedConferenceInvite { invite_call_id, .. } => call(*invite_call_id),
            Self::AbortReservedConferenceMute { .. } => CompletionKey::Configuration,
            Self::AbortReservedConferenceRemoval { .. } => CompletionKey::Configuration,
            Self::AbortReservedConferenceRoleChange { .. } => CompletionKey::Configuration,
            Self::AbortReservedHold { .. } => CompletionKey::Configuration,
            Self::AbortReservedJoin { call_id, .. } => call(*call_id),
            Self::AbortTransfer { device_id, .. } => CompletionKey::Device(device_id.clone()),
            Self::AbortVoicemail { device_id, .. } => CompletionKey::Device(device_id.clone()),
            Self::AcceptMediaReceive { device_id, .. } => CompletionKey::Device(device_id.clone()),
            Self::AcceptMediaTransmission { device_id, .. } => {
                CompletionKey::Device(device_id.clone())
            }
            Self::ApplyPartySnapshot { pbx_id, .. } => CompletionKey::Call(*pbx_id),
            Self::ApplyPickupIdentity { call_id, .. } => call(*call_id),
            Self::BeginVideoTransmitForDevice { device_id, .. } => {
                CompletionKey::Device(device_id.clone())
            }
            Self::CancelCallWaitingTone { .. } => CompletionKey::Configuration,
            Self::CancelFailedInboundOffer { pbx_id, .. } => CompletionKey::Call(*pbx_id),
            Self::CancelInboundOffer { call_id, .. } => call(*call_id),
            Self::CommitCallTransition { .. } => CompletionKey::Configuration,
            Self::CommitReservedConference { call_id, .. } => call(*call_id),
            Self::CommitReservedConferenceInvite { invite_call_id, .. } => call(*invite_call_id),
            Self::CommitReservedConferenceMute { .. } => CompletionKey::Configuration,
            Self::CommitReservedConferenceRemoval { .. } => CompletionKey::Configuration,
            Self::CommitReservedConferenceRoleChange { .. } => CompletionKey::Configuration,
            Self::CommitReservedHold { .. } => CompletionKey::Configuration,
            Self::CompensateUnrecordedCallTransitionEffect { .. } => CompletionKey::Configuration,
            Self::CompleteConferenceMutation { .. } => CompletionKey::Configuration,
            Self::CompleteDeviceTransfer { device_id, .. } => {
                CompletionKey::Device(device_id.clone())
            }
            Self::ActivateRemoteHangup { .. } => CompletionKey::Configuration,
            Self::CompleteRemoteHangupToken { .. } => CompletionKey::Configuration,
            Self::CompleteTransfer { device_id, .. } => CompletionKey::Device(device_id.clone()),
            Self::CompleteVoicemailNative { device_id, .. } => {
                CompletionKey::Device(device_id.clone())
            }
            Self::ConferenceDestinationFailed {
                handset_call_id, ..
            } => call(*handset_call_id),
            Self::ConferenceParticipantFailed { call_id, .. } => call(*call_id),
            Self::DrainConferencesForShutdown { .. } => CompletionKey::Configuration,
            Self::DrainOneWayMicrophones { .. } => CompletionKey::Configuration,
            Self::DrainRemoteHangups { .. } => CompletionKey::Configuration,
            Self::ExpireCallDeadlines { .. } => CompletionKey::Configuration,
            Self::ExpireRemoteHangups { .. } => CompletionKey::Configuration,
            Self::Hangup { call_id, .. } => call(*call_id),
            Self::InstallVideoPlanForDevice { device_id, .. } => {
                CompletionKey::Device(device_id.clone())
            }
            Self::MediaRetargetCompensationEnqueueFailed { call_id, .. } => call(*call_id),
            Self::MediaRetargetCompensationStarted { call_id, .. } => call(*call_id),
            Self::MediaRetargetEnqueueFailed { call_id, .. } => call(*call_id),
            Self::MediaRetargetStarted { call_id, .. } => call(*call_id),
            Self::PbxAnswer { pbx_id, .. } => CompletionKey::Call(*pbx_id),
            Self::PbxHangupWithEffects { pbx_id, .. } => CompletionKey::Call(*pbx_id),
            Self::PbxProceeding { pbx_id, .. } => CompletionKey::Call(*pbx_id),
            Self::PbxProgressWithMediaMode { pbx_id, .. } => CompletionKey::Call(*pbx_id),
            Self::PbxRinging { pbx_id, .. } => CompletionKey::Call(*pbx_id),
            Self::PhoneAnswer { call_id, .. } => call(*call_id),
            Self::PrepareDisconnect { device_id, .. } => CompletionKey::Device(device_id.clone()),
            Self::PreparePhoneHangup { call_id, .. } => call(*call_id),
            Self::PrepareRegisterSession { .. } => CompletionKey::Configuration,
            Self::PrepareRemoteHangup { pbx_id, .. } => CompletionKey::Call(*pbx_id),
            Self::RecordCallTransitionSuccess { .. } => CompletionKey::Configuration,
            Self::RecoverOptionalVideoEffectFailure { .. } => CompletionKey::Configuration,
            Self::RejectNativeCall {
                handset_call_id, ..
            } => call(*handset_call_id),
            Self::ReloadPolicy { .. } => CompletionKey::Configuration,
            Self::ScheduleReadyAutoAnswers { pbx_id, .. } => CompletionKey::Call(*pbx_id),
            Self::StartCallWaitingTone { .. } => CompletionKey::Configuration,
            Self::VideoFallbackForDevice { device_id, .. } => {
                CompletionKey::Device(device_id.clone())
            }
            Self::VideoReceiveOpenedForDevice { device_id, .. } => {
                CompletionKey::Device(device_id.clone())
            }
            Self::VideoTransmitOpenedForDevice { device_id, .. } => {
                CompletionKey::Device(device_id.clone())
            }
            Self::CommitDeviceFeatures { device_id, .. } => {
                CompletionKey::Device(device_id.clone())
            }
            Self::RetireCallRuntime { pbx_id, .. } => CompletionKey::Call(*pbx_id),
            Self::FinishNoAnswerRoute { pbx_id, .. } => CompletionKey::Call(*pbx_id),
            Self::CancelNoAnswerTimer { pbx_id, .. } => CompletionKey::Call(*pbx_id),
            Self::ExpireForwardingEntries { .. } => CompletionKey::Configuration,
            Self::SettleForwardingCollection { device_id, .. } => {
                CompletionKey::Device(device_id.clone())
            }
            Self::SettleForwardingTerminal { device_id, .. } => {
                CompletionKey::Device(device_id.clone())
            }
            Self::CancelForwardingForCall { device_id, .. } => {
                CompletionKey::Device(device_id.clone())
            }
            Self::BeginForwardingCommit { device_id, .. } => {
                CompletionKey::Device(device_id.clone())
            }
            Self::FinishForwardingCommit { device_id, .. } => {
                CompletionKey::Device(device_id.clone())
            }
            Self::CommitCodecMutation { .. } => CompletionKey::Configuration,
            Self::AbortCodecMutation { .. } => CompletionKey::Configuration,
            Self::CommitRegisteredFeatures { device_id, .. } => {
                CompletionKey::Device(device_id.clone())
            }
            _ => return None,
        })
    }
}
