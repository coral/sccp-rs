//! Backend-neutral SCCP call state used by the Asterisk adapter.
//!
//! This module contains no Asterisk pointers or constants. It is deliberately
//! small enough to test without loading a PBX and is the seam through which a
//! second PBX backend can reuse the handset state machine.
//!
//! PBX calls, handset appearances, and media directions have separate typed
//! identities and lifetimes. A physical inbound answer claims one appearance
//! and presents provisional station answer state before opening receive media,
//! but does not answer the PBX or publish the runtime Connected event until the
//! receive-channel acknowledgement. An outbound PBX answer publishes Connected
//! immediately; its ordinary media path then opens receive and starts transmit
//! in sequence.
//! Only an explicitly selected NAT early-progress transaction couples those
//! two wire requests, and its receive acknowledgement settles both protocol
//! directions before the runtime receives the corresponding typed events.
//!
//! Terminal transitions remove every call/appearance/media index before their
//! idempotent handset cleanup effects cross the backend boundary. Consequently
//! late media acknowledgements cannot resurrect a hung-up, transferred,
//! parked, shared, or conference-owned call.

use std::collections::{HashMap, HashSet};
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[cfg(test)]
use sccp_protocol::MediaCapability;
use sccp_protocol::{
    AppearanceRingMode, CallDirection, CallId, CallInfo, CallState as HandsetCallState, Codec,
    CodecKind, ConferenceId, ConferenceListEntry, DeviceId, DeviceRegistration, Digit,
    MediaEndpoint, MediaEndpointAddress, MultimediaPayload, ParticipantId, PassthroughPartyId,
    ProtocolVersion, SessionGeneration, StationMediaCapabilities, Tone,
};

use crate::call::auto_answer::AutoAnswerMode;
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
use crate::call::auto_answer::{AutoAnswerPolicy, AutoAnswerRequest};
use crate::call::forwarding::{ForwardingDestination, ForwardingRouteReason};
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
use crate::call::hotline::HotlineDestination;
use crate::call::metadata::{CallMetadata, MetadataError};
use crate::call::transfer::{
    DeferredTransferAction, TransferCancellationReason, TransferCompletion, TransferId,
    TransferLeg, TransferMode, TransferPhase, TransferRegistry, TransferRejection,
    TransferSetupMilestone, TransferSourceRecovery, TransferSourceState, TransferTransaction,
    TransferTrigger,
};
use crate::call::voicemail::{
    VoicemailAction, VoicemailPhase, VoicemailRegistry, VoicemailRejection, VoicemailTarget,
    VoicemailTransaction, VoicemailTransactionId,
};
use crate::conference::{
    ConferenceParticipant, ConferenceParticipantIdentity, ConferenceParticipantRegistry,
    MAX_CONFERENCE_PARTICIPANTS,
};
use crate::config::{LineBinding, LineDialToneConfig, VideoMode};
use crate::media::encryption::StationEncryptionCapabilities;
use crate::media::formats::OwnedNegotiatedVideo;
pub use crate::runtime::backend::PbxCallId;
use crate::runtime::backend::{
    BargeOperation, ConferenceAnnouncement, ConferenceAnnouncementOperation,
    ConferenceAnnouncementTarget, ConferenceDestinationOperation, DriverEffect, HandsetEffect,
    ParkingOperation, PbxBridgeId, PbxEffect, PickupOperation,
};

mod domains;
mod invariants;

/// Identity of one handset presentation of a PBX call.
///
/// This is intentionally distinct from the configured logical-line
/// appearance identifier exported by the protocol crate.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CallAppearanceId(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallState {
    Collecting,
    PickupCollecting,
    Ringing,
    Calling,
    Connected,
    Parking,
    Retrieving,
    Held,
    RemoteInUse,
    SharedHeld,
    Barged,
    TransferCollecting,
    Ended,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum OutboundCallPhase {
    Collecting,
    Routing,
    Proceeding,
    Ringing,
    Progress,
    Answered,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum OutboundIdentityStage {
    #[default]
    Awaiting,
    Ready,
    RingOutPublished,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallSwitchRejection {
    Unavailable,
    Conflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutoAnswerScheduleRejection {
    Unavailable,
    GenerationExhausted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HookFlashAction {
    AnswerWaiting(CallId),
    Transfer,
    Ignore,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) struct CallTransitionId(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
enum CallTransitionKind {
    Additional,
    Switch(CallState),
}

#[derive(Clone, Debug)]
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) struct CallTransition {
    pub id: CallTransitionId,
    pub effects: Vec<DriverEffect>,
    pub device_id: DeviceId,
    pub target_call_id: CallId,
    pub target_pbx_id: PbxCallId,
    previous_call_id: Option<CallId>,
    previous_pbx_id: Option<PbxCallId>,
    kind: CallTransitionKind,
    auto_answer_mode: Option<AutoAnswerMode>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
enum CallTransitionMilestone {
    PreviousBackendHeld,
    PreviousHandsetHeld,
    TargetBackendStarted,
    TargetHandsetChanged,
    TargetMicrophoneDisabled,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) struct CallTransitionProgress {
    completed: HashSet<CallTransitionMilestone>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) struct CallTransitionCompensation {
    pub effects: Vec<DriverEffect>,
    pub remove_target_channel: bool,
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
impl CallTransitionProgress {
    pub fn record_success(&mut self, transition: &CallTransition, effect: &DriverEffect) {
        match effect {
            DriverEffect::Backend(PbxEffect::Hold { call_id })
                if Some(*call_id) == transition.previous_pbx_id =>
            {
                self.completed
                    .insert(CallTransitionMilestone::PreviousBackendHeld);
            }
            DriverEffect::Handset(HandsetEffect::SetCallState {
                call_id,
                state: HandsetCallState::Hold,
                ..
            }) if Some(*call_id) == transition.previous_call_id => {
                self.completed
                    .insert(CallTransitionMilestone::PreviousHandsetHeld);
            }
            DriverEffect::Backend(
                PbxEffect::CreateChannel { call_id, .. }
                | PbxEffect::Answer { call_id }
                | PbxEffect::Resume { call_id },
            ) if *call_id == transition.target_pbx_id => {
                self.completed
                    .insert(CallTransitionMilestone::TargetBackendStarted);
            }
            DriverEffect::Handset(effect)
                if effect.transition_call_id() == Some(transition.target_call_id) =>
            {
                self.completed
                    .insert(CallTransitionMilestone::TargetHandsetChanged);
                if matches!(
                    effect,
                    HandsetEffect::SetMicrophoneMode { enabled: false, .. }
                ) {
                    self.completed
                        .insert(CallTransitionMilestone::TargetMicrophoneDisabled);
                }
            }
            _ => {}
        }
    }

    fn completed(&self, milestone: CallTransitionMilestone) -> bool {
        self.completed.contains(&milestone)
    }

    #[cfg(test)]
    fn with_completed(milestones: impl IntoIterator<Item = CallTransitionMilestone>) -> Self {
        Self {
            completed: milestones.into_iter().collect(),
        }
    }
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
impl CallTransition {
    pub fn remove_target_channel_on_abort(&self, progress: &CallTransitionProgress) -> bool {
        progress.completed(CallTransitionMilestone::TargetBackendStarted)
            && matches!(
                self.kind,
                CallTransitionKind::Additional | CallTransitionKind::Switch(CallState::Ringing)
            )
    }
}

#[derive(Clone)]
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
struct CallDomainSnapshot {
    devices: HashMap<DeviceId, RegisteredDevice>,
    pbx_calls: HashMap<PbxCallId, PbxCall>,
    appearances: HashMap<CallAppearanceId, CallAppearance>,
    appearance_by_sccp: HashMap<CallId, CallAppearanceId>,
    shared_control_claims: HashMap<PbxCallId, SharedControlClaim>,
    call_waiting_tones: HashMap<CallId, CallWaitingToneSchedule>,
    pending_phone_answers: HashMap<CallId, PbxCallId>,
    pending_route_media: HashSet<CallId>,
}

#[derive(Clone, Debug, Default)]
struct CallRegistry {
    pbx: HashMap<PbxCallId, PbxCall>,
    appearances: HashMap<CallAppearanceId, CallAppearance>,
    by_sccp: HashMap<CallId, CallAppearanceId>,
    by_device: HashMap<DeviceId, HashSet<CallAppearanceId>>,
}

#[derive(Clone)]
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
struct PendingCallTransition {
    transition: CallTransition,
    snapshot: CallDomainSnapshot,
    progress: CallTransitionProgress,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MediaStreamState {
    #[default]
    Closed,
    Opening,
    Open(MediaEndpoint),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoFallbackReason {
    NotNegotiated,
    NativeRtpUnavailable,
    LocalEndpointUnavailable,
    DescriptorUnavailable,
    ReceiveFailed,
    TransmitFailed,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum VideoStreamState {
    #[default]
    Closed,
    Opening,
    Open {
        codec: Codec,
        endpoint: MediaEndpointAddress,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoPlan {
    pub session_generation: SessionGeneration,
    pub protocol: ProtocolVersion,
    pub mode: VideoMode,
    pub negotiated: OwnedNegotiatedVideo,
    pub payload: MultimediaPayload,
    pub local_endpoint: MediaEndpointAddress,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VideoMediaState {
    AudioOnly(VideoFallbackReason),
    Blocked {
        plan: VideoPlan,
        reason: VideoFallbackReason,
    },
    Ready {
        plan: VideoPlan,
        receive: VideoStreamState,
        transmit: VideoStreamState,
        transmit_token: Option<PassthroughPartyId>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoPlanReadiness {
    Ready,
    Blocked(VideoFallbackReason),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VideoFallbackOutcome {
    Ignored,
    Applied { cleanup: Option<VideoCleanup> },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoCleanup {
    pub device_id: DeviceId,
    pub call_id: CallId,
    pub session_generation: SessionGeneration,
}

impl From<VideoCleanup> for HandsetEffect {
    fn from(cleanup: VideoCleanup) -> Self {
        Self::StopVideo {
            device_id: cleanup.device_id,
            call_id: cleanup.call_id,
            session_generation: cleanup.session_generation,
        }
    }
}

impl VideoFallbackOutcome {
    pub const fn is_applied(&self) -> bool {
        matches!(self, Self::Applied { .. })
    }

    pub fn into_effects(self) -> Vec<DriverEffect> {
        match self {
            Self::Ignored => Vec::new(),
            Self::Applied { cleanup } => cleanup
                .into_iter()
                .map(HandsetEffect::from)
                .map(DriverEffect::from)
                .collect(),
        }
    }
}

impl Default for VideoMediaState {
    fn default() -> Self {
        Self::audio_only(VideoFallbackReason::NotNegotiated)
    }
}

impl VideoMediaState {
    pub const fn audio_only(reason: VideoFallbackReason) -> Self {
        Self::AudioOnly(reason)
    }

    pub fn blocked(plan: VideoPlan, reason: VideoFallbackReason) -> Self {
        Self::Blocked { plan, reason }
    }

    pub fn ready(plan: VideoPlan) -> Self {
        Self::Ready {
            plan,
            receive: VideoStreamState::Closed,
            transmit: VideoStreamState::Closed,
            transmit_token: None,
        }
    }

    pub const fn plan(&self) -> Option<&VideoPlan> {
        match self {
            Self::AudioOnly(_) => None,
            Self::Blocked { plan, .. } | Self::Ready { plan, .. } => Some(plan),
        }
    }

    pub const fn receive(&self) -> VideoStreamState {
        match self {
            Self::Ready { receive, .. } => *receive,
            Self::AudioOnly(_) | Self::Blocked { .. } => VideoStreamState::Closed,
        }
    }

    pub const fn transmit(&self) -> VideoStreamState {
        match self {
            Self::Ready { transmit, .. } => *transmit,
            Self::AudioOnly(_) | Self::Blocked { .. } => VideoStreamState::Closed,
        }
    }

    pub const fn fallback_reason(&self) -> Option<VideoFallbackReason> {
        match self {
            Self::AudioOnly(reason) | Self::Blocked { reason, .. } => Some(*reason),
            Self::Ready { .. } => None,
        }
    }

    pub const fn is_idle(&self) -> bool {
        match self {
            Self::Ready {
                receive, transmit, ..
            } => {
                matches!(receive, VideoStreamState::Closed)
                    && matches!(transmit, VideoStreamState::Closed)
            }
            Self::AudioOnly(_) | Self::Blocked { .. } => true,
        }
    }

    fn accepts_failure(&self, reason: VideoFallbackReason) -> bool {
        match reason {
            VideoFallbackReason::ReceiveFailed => matches!(
                self.receive(),
                VideoStreamState::Opening | VideoStreamState::Open { .. }
            ),
            VideoFallbackReason::TransmitFailed => matches!(
                self.transmit(),
                VideoStreamState::Opening | VideoStreamState::Open { .. }
            ),
            VideoFallbackReason::NotNegotiated
            | VideoFallbackReason::DescriptorUnavailable
            | VideoFallbackReason::NativeRtpUnavailable
            | VideoFallbackReason::LocalEndpointUnavailable => true,
        }
    }

    pub fn close_streams(&mut self) {
        if let Self::Ready {
            receive,
            transmit,
            transmit_token,
            ..
        } = self
        {
            *receive = VideoStreamState::Closed;
            *transmit = VideoStreamState::Closed;
            *transmit_token = None;
        }
    }

    fn cleanup(&self, device_id: &DeviceId, call_id: CallId) -> Option<VideoCleanup> {
        (!self.is_idle()).then(|| VideoCleanup {
            device_id: device_id.clone(),
            call_id,
            session_generation: self
                .plan()
                .expect("non-idle video state retains its plan")
                .session_generation,
        })
    }

    fn begin_receive(&mut self) -> bool {
        match self {
            Self::Ready {
                receive, transmit, ..
            } if matches!(receive, VideoStreamState::Closed)
                && matches!(transmit, VideoStreamState::Closed) =>
            {
                *receive = VideoStreamState::Opening;
                true
            }
            Self::AudioOnly(_) | Self::Blocked { .. } | Self::Ready { .. } => false,
        }
    }

    fn opened_receive(&mut self, codec: Codec, endpoint: MediaEndpointAddress) -> bool {
        if let Self::Ready { plan, receive, .. } = self
            && matches!(receive, VideoStreamState::Opening)
            && plan.negotiated.codec() == codec
        {
            *receive = VideoStreamState::Open { codec, endpoint };
            true
        } else {
            false
        }
    }

    fn begin_transmit(&mut self) -> bool {
        match self {
            Self::Ready {
                receive, transmit, ..
            } if matches!(receive, VideoStreamState::Open { .. })
                && matches!(transmit, VideoStreamState::Closed) =>
            {
                *transmit = VideoStreamState::Opening;
                true
            }
            Self::AudioOnly(_) | Self::Blocked { .. } | Self::Ready { .. } => false,
        }
    }

    fn opened_transmit(
        &mut self,
        codec: Codec,
        endpoint: MediaEndpointAddress,
        token: PassthroughPartyId,
    ) -> bool {
        if let Self::Ready {
            plan,
            transmit,
            transmit_token,
            ..
        } = self
            && matches!(transmit, VideoStreamState::Opening)
            && plan.negotiated.codec() == codec
        {
            *transmit = VideoStreamState::Open { codec, endpoint };
            *transmit_token = Some(token);
            true
        } else {
            false
        }
    }
}

/// Wire strategy for opening an outbound handset media path.
///
/// A staged path opens receive media first and starts transmission only after
/// the handset acknowledges it. A coupled path is reserved for a station for
/// which the adapter has positively selected NAT traversal: both requests are
/// written without an acknowledgement boundary because older 79x1 firmware
/// can otherwise withhold the receive acknowledgement indefinitely.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OutboundMediaMode {
    #[default]
    Staged,
    Coupled,
}

#[derive(Clone, Debug)]
pub struct CallSnapshot {
    pub sccp_id: CallId,
    pub pbx_id: PbxCallId,
    pub device_id: DeviceId,
    pub line_instance: u32,
    pub line: String,
    pub direction: CallDirection,
    pub state: CallState,
    pub digits: String,
    pub info: CallInfo,
    pub metadata: CallMetadata,
    pub codec: Codec,
    /// Handset receive-channel lifecycle and acknowledged RTP endpoint.
    pub audio: MediaStreamState,
    /// Handset transmit-channel lifecycle and acknowledged RTP endpoint.
    pub audio_transmit: MediaStreamState,
    pub video: VideoMediaState,
    digit_deadline: Option<Instant>,
}

/// State owned by one PBX channel, independent of how many phones display it.
#[derive(Clone, Debug)]
pub struct PbxCall {
    pub id: PbxCallId,
    pub line: String,
    pub context: String,
    pub direction: CallDirection,
    pub state: CallState,
    outbound_phase: Option<OutboundCallPhase>,
    outbound_identity_stage: OutboundIdentityStage,
    pub digits: String,
    /// Runtime privacy for this call. Once enabled it protects every shared
    /// presentation until the owning handset explicitly disables it.
    pub privacy: bool,
    pub metadata: CallMetadata,
    pending_pickup: Option<PendingDirectedPickup>,
    appearance_ids: Vec<CallAppearanceId>,
    active_appearance: Option<CallAppearanceId>,
    digit_deadline: Option<Instant>,
    last_digit_at: Option<Instant>,
    simulated_enbloc_eligible: bool,
    overlap_enabled: bool,
}

#[derive(Clone, Debug)]
struct PendingDirectedPickup {
    context: String,
    answer: bool,
}

impl PbxCall {
    pub fn appearance_ids(&self) -> impl Iterator<Item = CallAppearanceId> + '_ {
        self.appearance_ids.iter().copied()
    }

    pub fn active_appearance(&self) -> Option<CallAppearanceId> {
        self.active_appearance
    }
}

/// One phone's independently addressable presentation of a PBX call.
#[derive(Clone, Debug)]
pub struct CallAppearance {
    pub id: CallAppearanceId,
    pub sccp_id: CallId,
    pub pbx_id: PbxCallId,
    pub device_id: DeviceId,
    pub line_instance: u32,
    pub state: CallState,
    pub ring_mode: AppearanceRingMode,
    /// Privacy configured specifically for this logical-line appearance.
    pub privacy: bool,
    /// Handset-visible party metadata for this specific appearance.
    pub info: CallInfo,
    pub codec: Codec,
    /// Handset receive-channel lifecycle and acknowledged RTP endpoint.
    pub audio: MediaStreamState,
    /// Handset transmit-channel lifecycle and acknowledged RTP endpoint.
    pub audio_transmit: MediaStreamState,
    pub video: VideoMediaState,
    /// Captured presentation policy for an auto-answered intercom call.
    /// Only one-way calls retain device-microphone ownership after commit.
    auto_answer_mode: Option<AutoAnswerMode>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BargeMode {
    Directed,
    Conference,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BargeRejection {
    Unavailable,
    NotRemote,
    Private,
    Capability,
    Conflict,
    AlreadyBarged,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PickupRejection {
    Unavailable,
    Permission,
    Disabled,
    Conflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParkingRejection {
    Unavailable,
    Disabled,
    Conflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConferenceRejection {
    Unavailable,
    Disabled,
    Conflict,
    NotConnected,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConferenceDestinationRequest {
    pub device_id: DeviceId,
    pub handset_call_id: CallId,
    pub destination: String,
    pub application_options: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConferenceDestinationRejection {
    Unavailable,
    Conflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConferenceParticipantRejection {
    Unavailable,
    NotModerator,
    InvalidParticipant,
    Moderator,
    LastModerator,
    Conflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConferenceEndRejection {
    Unavailable,
    NotModerator,
    Conflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConferencePhase {
    Consultation,
    Merging,
    Active,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConferenceOrigin {
    Consultation,
    Selection,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConferenceInvite {
    pub moderator_id: ParticipantId,
    pub moderator_call_id: PbxCallId,
    pub music_started: bool,
    pub participant: ConferenceParticipant,
}

/// Normalized media behavior captured when a conference is created. Keeping
/// this policy with the conference makes later invite and participant actions
/// independent of configuration reloads.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConferenceMediaPolicy {
    pub music_on_hold_class: Option<String>,
    pub mute_on_entry: bool,
    pub play_general_announcements: bool,
    pub play_participant_announcements: bool,
}

#[derive(Clone, Debug)]
pub struct ConferenceConsultationRequest {
    pub original_call_id: CallId,
    pub consultation_call_id: CallId,
    pub binding: LineBinding,
    pub codec: Codec,
    pub now: Instant,
    pub permitted: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConferenceParticipantMutationKind {
    Mute(bool),
    Remove,
    Moderator(bool),
    Hold(bool),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConferenceParticipantMutation {
    pub participant_id: ParticipantId,
    pub call_id: PbxCallId,
    pub kind: ConferenceParticipantMutationKind,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum ConferenceMutationOwner {
    Session(ConferenceId),
    Destination(PbxCallId),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ConferenceMutationToken {
    owner: ConferenceMutationOwner,
    generation: u64,
}

#[cfg(test)]
impl ConferenceMutationToken {
    pub(crate) const fn for_test(call_id: PbxCallId) -> Self {
        Self {
            owner: ConferenceMutationOwner::Destination(call_id),
            generation: 1,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConferenceSession {
    pub id: ConferenceId,
    pub bridge_id: PbxBridgeId,
    pub device_id: DeviceId,
    pub original_handset_call_id: CallId,
    pub original_call_id: PbxCallId,
    pub consultation_handset_call_id: CallId,
    pub consultation_call_id: PbxCallId,
    pub phase: ConferencePhase,
    pub origin: ConferenceOrigin,
    pub participants: ConferenceParticipantRegistry,
    pub media_policy: ConferenceMediaPolicy,
    pub pending_invite: Option<ConferenceInvite>,
    pub pending_participant_mutation: Option<ConferenceParticipantMutation>,
}

impl ConferenceSession {
    pub fn list_effect(&self, call_id: CallId) -> HandsetEffect {
        HandsetEffect::ShowConferenceList {
            device_id: self.device_id.clone(),
            call_id,
            conference_id: self.id,
            participants: self
                .participants
                .iter()
                .map(conference_list_entry)
                .collect(),
        }
    }

    pub fn participant_actions_effect(
        &self,
        participant_id: ParticipantId,
    ) -> Option<HandsetEffect> {
        let participant = self.participants.get(participant_id)?;
        let moderator_count = self.participants.moderator_count();
        if participant.moderator && moderator_count == 1 {
            return None;
        }
        Some(HandsetEffect::ShowConferenceParticipantActions {
            device_id: self.device_id.clone(),
            call_id: self.original_handset_call_id,
            conference_id: self.id,
            participant: conference_list_entry(participant),
            removable: !participant.moderator && self.participants.iter().len() > 2,
            demotable: participant.moderator && moderator_count > 1,
        })
    }
}

fn conference_list_entry(participant: &ConferenceParticipant) -> ConferenceListEntry {
    ConferenceListEntry {
        participant_id: participant.id,
        name: participant.display_name.clone(),
        number: participant.number.clone(),
        moderator: participant.moderator,
        muted: participant.muted,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BargeSession {
    pub target_call_id: PbxCallId,
    pub barger_call_id: PbxCallId,
    pub bridge_id: PbxBridgeId,
    pub handset_call_id: CallId,
    pub mode: BargeMode,
}

#[derive(Clone, Debug)]
struct BargeGroup {
    bridge_id: PbxBridgeId,
    mode: BargeMode,
    members: Vec<CallId>,
}

#[derive(Clone, Debug, Default)]
struct ConferenceRegistry {
    by_consultation: HashMap<CallId, ConferenceSession>,
    by_pbx: HashMap<PbxCallId, CallId>,
}

#[derive(Clone, Debug, Default)]
struct BargeRegistry {
    groups: HashMap<PbxCallId, BargeGroup>,
    by_handset: HashMap<CallId, BargeSession>,
    by_pbx: HashMap<PbxCallId, CallId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SharedControlClaim {
    Steal(CallAppearanceId),
    Barge(PbxBridgeId),
}

/// One pre-allocated handset identity and configured line appearance that may
/// receive an inbound PBX call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InboundAppearance {
    pub call_id: CallId,
    pub binding: LineBinding,
    pub codec: Codec,
}

/// An eligible inbound handset offer, retained in deterministic configuration
/// order. The adapter is responsible for creating the session call and
/// honoring the configured audible-ring policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InboundOffer {
    pub device_id: DeviceId,
    pub line_instance: u32,
    pub call_id: CallId,
    pub ring_mode: AppearanceRingMode,
    pub state: HandsetCallState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CallWaitingToneSchedule {
    device_id: DeviceId,
    waiting_call_id: CallId,
    active_call_id: CallId,
    tone: Tone,
    interval: Duration,
    next_at: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
struct PendingAutoAnswer {
    generation: u64,
    pbx_id: PbxCallId,
    call_id: CallId,
    deadline: Instant,
    request: AutoAnswerRequest,
    tone: Tone,
}

/// Result of applying per-device shared-line routing policy before a PBX call
/// is presented to handsets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InboundCallDisposition {
    Offer(Vec<InboundOffer>),
    Forward {
        binding: Box<LineBinding>,
        destination: ForwardingDestination,
        reason: ForwardingRouteReason,
    },
    Unavailable(InboundUnavailableReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InboundUnavailableReason {
    Conflict,
    NoEligibleAppearance,
    DoNotDisturb,
    ForwardingConflict,
    IncomingLimit,
}

/// Controller output for a PBX hangup. Session adapters should publish every
/// availability effect before discarding their handset-side call objects.
#[derive(Clone, Debug)]
pub struct PbxHangupOutcome {
    pub primary: Option<CallSnapshot>,
    pub effects: Vec<DriverEffect>,
}

/// Generation-scoped ownership of one delayed handset cleanup after the PBX
/// channel has already been detached. Tokens never wrap, so a stale timer can
/// neither close a later call nor consume a replacement notification.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) struct RemoteHangupToken(u64);

#[derive(Clone, Debug)]
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
struct PendingRemoteHangup {
    token: RemoteHangupToken,
    device_id: DeviceId,
    call_id: CallId,
    deadline: Instant,
}

/// Immediate PBX teardown plus optional ownership of the short handset tone
/// presentation which remains after native channel cleanup.
#[derive(Clone, Debug)]
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) struct RemoteHangupPlan {
    pub outcome: PbxHangupOutcome,
    pub pending: Option<RemoteHangupToken>,
}

/// One atomically detached conference and the terminal work required to
/// release its backend and handset resources. Call identifiers are retained
/// separately so an adapter can release its owned channel references even if
/// one of the best-effort hangup effects fails.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConferenceCleanupPlan {
    pub conference_id: ConferenceId,
    pub call_ids: Vec<PbxCallId>,
    pub effects: Vec<DriverEffect>,
}

/// Committed cleanup after one handset presentation failed. A surviving
/// session keeps its stable conference, bridge, and participant identities;
/// otherwise the effects contain the complete terminal cleanup sequence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConferenceParticipantFailureOutcome {
    pub conference_id: ConferenceId,
    pub failed_call_id: PbxCallId,
    pub call_ids: Vec<PbxCallId>,
    pub surviving_session: Option<ConferenceSession>,
    pub effects: Vec<DriverEffect>,
}

#[derive(Clone, Debug)]
pub struct TransferConsultationRequest {
    pub source_call_id: CallId,
    pub consultation_call_id: CallId,
    pub binding: LineBinding,
    pub codec: Codec,
    pub complete_on_hangup: bool,
    pub now: Instant,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferCompletionPlan {
    pub completion: TransferCompletion,
    pub effects: Vec<DriverEffect>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferTerminalOutcome {
    pub transaction: TransferTransaction,
    pub effects: Vec<DriverEffect>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VoicemailPlan {
    pub transaction: VoicemailTransaction,
    pub effects: Vec<DriverEffect>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VoicemailTerminalOutcome {
    pub transaction: VoicemailTransaction,
    pub effects: Vec<DriverEffect>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VoicemailNativeOutcome {
    Committed(VoicemailTerminalOutcome),
    CallAlreadyEnded,
}

#[derive(Clone, Debug)]
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) struct HotlineCallRequest {
    pub handset_call_id: CallId,
    pub binding: LineBinding,
    pub codec: Codec,
    pub destination: HotlineDestination,
    pub now: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodecPreferenceRejection {
    Unavailable,
    NotPreDial,
    Ambiguous,
}

#[derive(Clone, Debug)]
pub struct RegisteredDevice {
    pub registration: DeviceRegistration,
    pub session_generation: SessionGeneration,
    pub capabilities: Option<StationMediaCapabilities>,
    pub audio_encryption: StationEncryptionCapabilities,
    pub selected_line: Option<u32>,
    active_call: Option<CallId>,
    selected_calls: HashSet<CallId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegisterSessionOutcome {
    pub cleanup: Vec<DriverEffect>,
    pub replaced: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DndMode {
    #[default]
    Off,
    Silent,
    Reject,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ForwardingState {
    pub all: Option<ForwardingDestination>,
    pub busy: Option<ForwardingDestination>,
    pub no_answer: Option<ForwardingDestination>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DeviceFeatureState {
    pub dnd: DndMode,
    pub privacy: bool,
    /// Device-wide opt-in for recording the next eligible owned call.
    ///
    /// Active recording state is deliberately kept out of persisted feature
    /// state and lives only in the runtime recording registry.
    pub recording_armed: bool,
    pub forwarding: ForwardingState,
    pub buttons: HashMap<u32, bool>,
}

impl RegisteredDevice {
    pub fn active_call(&self) -> Option<CallId> {
        self.active_call
    }

    pub fn selected_calls(&self) -> impl Iterator<Item = CallId> + '_ {
        self.selected_calls.iter().copied()
    }

    pub fn is_call_selected(&self, call_id: CallId) -> bool {
        self.selected_calls.contains(&call_id)
    }
}

pub struct Controller {
    next_pbx_id: u64,
    next_appearance_id: u64,
    next_bridge_id: u64,
    next_conference_id: u32,
    next_participant_id: u32,
    next_conference_mutation_generation: u64,
    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    next_call_transition_id: u64,
    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    next_auto_answer_generation: u64,
    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    next_remote_hangup_generation: u64,
    first_digit: Duration,
    interdigit: Duration,
    dial_terminator: char,
    simulate_enbloc: bool,
    overlap_devices: HashSet<DeviceId>,
    line_dial_tones: HashMap<String, LineDialToneConfig>,
    line_incoming_limits: HashMap<String, u32>,
    call_waiting_tones: HashMap<CallId, CallWaitingToneSchedule>,
    /// Inbound handset answers which have claimed an appearance but have not
    /// yet been committed to the PBX because OpenReceiveChannel is pending.
    pending_phone_answers: HashMap<CallId, PbxCallId>,
    /// Outbound coupled ORC/SMT transactions which have not received an ORC
    /// acknowledgement yet. The transmit acknowledgement is independent.
    pending_route_media: HashSet<CallId>,
    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pending_call_transitions: HashMap<CallTransitionId, PendingCallTransition>,
    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    auto_answer_requests: HashMap<PbxCallId, AutoAnswerRequest>,
    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pending_auto_answers: HashMap<CallId, PendingAutoAnswer>,
    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pending_remote_hangups: HashMap<CallId, PendingRemoteHangup>,
    devices: HashMap<DeviceId, RegisteredDevice>,
    features: HashMap<DeviceId, DeviceFeatureState>,
    call_registry: CallRegistry,
    shared_control_claims: HashMap<PbxCallId, SharedControlClaim>,
    barges: BargeRegistry,
    conferences: ConferenceRegistry,
    conference_mutations: HashMap<ConferenceMutationOwner, u64>,
    transfers: TransferRegistry,
    voicemail: VoicemailRegistry,
    redirect_claims: HashSet<PbxCallId>,
}

/// Runs one pure controller transition and drops the mutex guard before
/// returning its owned result to adapter code. Adapter I/O belongs after this
/// function returns.
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) fn controller_step<T>(
    controller: &Mutex<Controller>,
    step: impl FnOnce(&mut Controller) -> T,
) -> T {
    let mut controller = controller.lock().expect("SCCP controller lock poisoned");
    step(&mut controller)
}

impl Controller {
    pub fn new(interdigit: Duration) -> Self {
        Self::with_digit_timeouts(interdigit, interdigit)
    }

    /// Applies the fast-keypad deadline policy to future digits. Disabling it
    /// leaves existing absolute deadlines unchanged.
    pub fn set_simulated_enbloc(&mut self, enabled: bool) {
        self.simulate_enbloc = enabled;
    }

    /// Replaces the logical-line limit used for future inbound admission.
    /// Existing calls are never evicted when a reload lowers a limit.
    pub fn set_line_incoming_limits(&mut self, lines: impl IntoIterator<Item = (String, u32)>) {
        self.line_incoming_limits = lines.into_iter().collect();
    }

    /// Replaces media capabilities only for the connection that advertised
    /// them. A late update from a replaced connection has no effect.
    pub fn update_capabilities(
        &mut self,
        device: &DeviceId,
        session_generation: SessionGeneration,
        capabilities: StationMediaCapabilities,
    ) -> bool {
        let Some(state) = self
            .devices
            .get_mut(device)
            .filter(|state| state.session_generation == session_generation)
        else {
            return false;
        };
        state.capabilities = Some(capabilities);
        true
    }

    /// Replaces audio-encryption advertisements only for the connection that
    /// supplied them.
    pub fn update_audio_encryption_capabilities(
        &mut self,
        device: &DeviceId,
        session_generation: SessionGeneration,
        capabilities: StationEncryptionCapabilities,
    ) -> bool {
        let Some(state) = self
            .devices
            .get_mut(device)
            .filter(|state| state.session_generation == session_generation)
        else {
            return false;
        };
        state.audio_encryption = capabilities;
        true
    }

    #[cfg(test)]
    pub fn registered(&mut self, registration: DeviceRegistration) {
        let next = self.devices.get(&registration.id).map_or(1, |state| {
            state
                .session_generation
                .get()
                .checked_add(1)
                .expect("test session generation is available")
        });
        let generation = SessionGeneration::new(next).expect("test session generation is nonzero");
        let _ = self.register_session(generation, registration);
    }

    #[cfg(test)]
    pub fn capabilities(
        &mut self,
        device: &DeviceId,
        capabilities: Vec<sccp_protocol::MediaCapability>,
    ) {
        let generation = self
            .devices
            .get(device)
            .expect("test device must be registered")
            .session_generation;
        assert!(self.update_capabilities(device, generation, capabilities.into()));
    }

    pub fn disconnected(&mut self, device: &DeviceId) -> Vec<DriverEffect> {
        #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
        self.pending_call_transitions
            .retain(|_, pending| &pending.transition.device_id != device);
        #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
        {
            let removed = self
                .appearances_for_device(device)
                .map(|appearance| appearance.sccp_id)
                .collect::<HashSet<_>>();
            self.pending_auto_answers
                .retain(|call_id, _| !removed.contains(call_id));
        }
        #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
        self.pending_remote_hangups
            .retain(|_, pending| &pending.device_id != device);
        let mut pending_answers = self
            .appearances_for_device(device)
            .filter(|appearance| {
                self.pending_phone_answers.get(&appearance.sccp_id) == Some(&appearance.pbx_id)
            })
            .map(|appearance| appearance.sccp_id)
            .collect::<Vec<_>>();
        pending_answers.sort_unstable_by_key(|call_id| call_id.0);
        let transfer = self.transfers.get(device).cloned();
        let conferences: Vec<_> = self
            .conferences
            .by_consultation
            .values()
            .filter(|session| {
                session
                    .participants
                    .iter()
                    .any(|participant| &participant.device_id == device)
            })
            .cloned()
            .collect();
        let barges: Vec<_> = self
            .call_registry
            .by_device
            .get(device)
            .into_iter()
            .flatten()
            .filter_map(|appearance_id| self.call_registry.appearances.get(appearance_id))
            .map(|appearance| appearance.sccp_id)
            .filter(|call_id| self.barges.by_handset.contains_key(call_id))
            .collect();
        let mut actions = Vec::new();
        if let Some(transfer) = transfer
            && let Ok(outcome) = self.abort_transfer(
                device,
                transfer.id,
                TransferCancellationReason::DeviceDisconnect,
            )
        {
            actions.extend(outcome.effects);
        }
        for session in conferences {
            if session.phase == ConferencePhase::Active {
                let departing_calls = session
                    .participants
                    .iter()
                    .filter(|participant| &participant.device_id == device)
                    .map(|participant| participant.pbx_call_id)
                    .collect::<Vec<_>>();
                for pbx_id in departing_calls {
                    let Some(current) = self.conference_session_by_pbx(pbx_id).cloned() else {
                        continue;
                    };
                    actions.extend(self.active_conference_departure(current, pbx_id, None, false));
                }
            } else {
                let bridge_created = matches!(session.phase, ConferencePhase::Merging);
                actions.extend(self.end_conference_internal(session, bridge_created, None));
            }
        }
        for call_id in barges {
            actions.extend(self.end_barge_internal(call_id, true, true, false));
        }
        // A handset answer does not answer the Asterisk channel until its
        // receive-channel acknowledgement commits media. Losing that exact
        // owner is therefore an answer failure, not an established shared
        // call that another appearance can steal.
        for call_id in pending_answers {
            actions.extend(self.terminate(call_id));
        }
        self.devices.remove(device);
        let appearance_ids: Vec<_> = self
            .call_registry
            .by_device
            .get(device)
            .into_iter()
            .flatten()
            .copied()
            .collect();
        let mut empty_calls = HashSet::new();
        for appearance_id in appearance_ids {
            if let Some(appearance) = self.remove_appearance(appearance_id)
                && self
                    .call_registry
                    .pbx
                    .get(&appearance.pbx_id)
                    .is_some_and(|call| call.appearance_ids.is_empty())
            {
                empty_calls.insert(appearance.pbx_id);
            }
        }
        for pbx_id in empty_calls {
            actions.extend(self.end_barges_for_target(pbx_id));
            if self.remove_pbx_call(pbx_id).is_some() {
                actions.push(PbxEffect::Hangup { call_id: pbx_id }.into());
            }
        }
        actions.retain(|effect| {
            !matches!(
                effect,
                DriverEffect::Handset(effect) if effect.device_id() == device
            )
        });
        debug_assert!(self.invariant_error().is_none());
        actions
    }

    pub fn is_registered(&self, device: &DeviceId) -> bool {
        self.devices.contains_key(device)
    }

    pub fn set_privacy(&mut self, device: &DeviceId, enabled: bool) {
        self.feature_state_mut(device).privacy = enabled;
    }

    pub fn select_line(&mut self, device: &DeviceId, line_instance: u32) -> bool {
        let Some(state) = self.devices.get_mut(device) else {
            return false;
        };
        state.selected_line = Some(line_instance);
        true
    }

    /// Resolve a hook flash against the exact active handset identity without
    /// mutating call state. A waiting inbound call takes precedence over
    /// starting a consultation transfer; existing transfer consultations use
    /// the same action so a second flash can complete that transaction.
    pub fn hook_flash_action(&self, device_id: &DeviceId, call_id: CallId) -> HookFlashAction {
        let Some(device) = self.devices.get(device_id) else {
            return HookFlashAction::Ignore;
        };
        if device.active_call != Some(call_id) {
            return HookFlashAction::Ignore;
        }
        if let Some(transfer) = self.transfers.get(device_id) {
            return if transfer
                .consultation
                .is_some_and(|leg| leg.handset_call_id == call_id)
            {
                HookFlashAction::Transfer
            } else {
                HookFlashAction::Ignore
            };
        }
        let Some(active) = self.appearance_for_call(call_id) else {
            return HookFlashAction::Ignore;
        };
        if active.state != CallState::Connected
            || self.conferences.by_pbx.contains_key(&active.pbx_id)
            || self.barges.by_handset.contains_key(&call_id)
        {
            return HookFlashAction::Ignore;
        }
        let mut waiting = self
            .appearances_for_device(device_id)
            .filter(|appearance| {
                appearance.sccp_id != call_id && appearance.state == CallState::Ringing
            })
            .map(|appearance| appearance.sccp_id)
            .collect::<Vec<_>>();
        waiting.sort_by_key(|waiting_call_id| waiting_call_id.0);
        waiting
            .first()
            .copied()
            .map_or(HookFlashAction::Transfer, HookFlashAction::AnswerWaiting)
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    fn restored_target_handset_effects(&self, transition: &CallTransition) -> Vec<DriverEffect> {
        let Some(appearance) = self.appearance_for_call(transition.target_call_id).cloned() else {
            return vec![
                HandsetEffect::SetCallState {
                    device_id: transition.device_id.clone(),
                    call_id: transition.target_call_id,
                    state: HandsetCallState::OnHook,
                    stop_media: true,
                }
                .into(),
            ];
        };
        let (state, stop_media) = match appearance.state {
            CallState::Ringing => (self.inbound_offer_for_appearance(&appearance).state, false),
            CallState::Held => (HandsetCallState::Hold, true),
            CallState::SharedHeld => (HandsetCallState::HoldRed, true),
            CallState::Connected => (HandsetCallState::Connected, false),
            CallState::RemoteInUse => (HandsetCallState::RemoteMultiline, true),
            _ => (HandsetCallState::OnHook, true),
        };
        vec![appearance_state_effect(&appearance, state, stop_media)]
    }

    pub fn enbloc(&mut self, call_id: CallId, number: String) -> Vec<DriverEffect> {
        let Some((pbx_id, state)) = self
            .appearance_for_call(call_id)
            .map(|appearance| (appearance.pbx_id, appearance.state))
        else {
            return Vec::new();
        };
        if matches!(state, CallState::Connected | CallState::Held) {
            let digits = number
                .chars()
                .map(|character| match character {
                    '0'..='9' | '*' | '#' | 'A'..='D' => Some(character),
                    'a'..='d' => Some(character.to_ascii_uppercase()),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>();
            return digits.map_or_else(Vec::new, |digits| {
                digits
                    .into_iter()
                    .map(|digit| {
                        PbxEffect::SendDigit {
                            call_id: pbx_id,
                            digit,
                        }
                        .into()
                    })
                    .collect()
            });
        }
        if !matches!(
            state,
            CallState::Collecting | CallState::PickupCollecting | CallState::TransferCollecting
        ) {
            return Vec::new();
        }
        // Some phones send the Dial soft key without repeating the digits
        // already delivered as KeypadButton messages.
        if !number.is_empty()
            && let Some(call) = self.call_registry.pbx.get_mut(&pbx_id)
        {
            call.digits = number;
        }
        self.finish_digits(call_id)
    }

    /// Publish pre-answer progress and open media once when policy permits it.
    pub fn pbx_progress(&mut self, pbx_id: PbxCallId, early_media: bool) -> Vec<DriverEffect> {
        self.pbx_progress_with_media_mode(pbx_id, early_media, OutboundMediaMode::Staged)
    }

    pub fn pbx_proceeding(&mut self, pbx_id: PbxCallId) -> Vec<DriverEffect> {
        let Some(appearance) =
            self.advance_outbound_call_phase(pbx_id, OutboundCallPhase::Proceeding)
        else {
            return Vec::new();
        };
        vec![
            HandsetEffect::PresentOutboundProceeding {
                device_id: appearance.device_id,
                call_id: appearance.sccp_id,
                info: appearance.info,
            }
            .into(),
        ]
    }

    /// Move an active shared call to another registered presentation without
    /// answering or replacing the PBX channel.
    pub fn steal(&mut self, call_id: CallId) -> Vec<DriverEffect> {
        let Some(requester) = self.appearance_for_call(call_id).cloned() else {
            return Vec::new();
        };
        let Some(call) = self.call_registry.pbx.get(&requester.pbx_id) else {
            return Vec::new();
        };
        if self.redirect_claims.contains(&requester.pbx_id)
            || self
                .pending_phone_answers
                .values()
                .any(|pending_pbx_id| *pending_pbx_id == requester.pbx_id)
            || call.state != CallState::Connected
            || requester.state != CallState::RemoteInUse
            || call.active_appearance == Some(requester.id)
            || !self.shared_control_eligible(&requester)
        {
            return Vec::new();
        }
        let previous_owner = call.active_appearance;
        let appearance_ids = call.appearance_ids.clone();
        let requester_privacy = requester.privacy
            || self
                .features
                .get(&requester.device_id)
                .is_some_and(|state| state.privacy);
        if let Some(call) = self.call_registry.pbx.get_mut(&requester.pbx_id) {
            call.active_appearance = Some(requester.id);
            call.privacy |= requester_privacy;
        }
        self.shared_control_claims
            .insert(requester.pbx_id, SharedControlClaim::Steal(requester.id));
        let mut effects = Vec::new();
        for appearance_id in appearance_ids {
            let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) else {
                continue;
            };
            if appearance_id == requester.id {
                appearance.state = CallState::Connected;
            } else {
                appearance.state = CallState::RemoteInUse;
                if previous_owner == Some(appearance_id) {
                    effects.push(appearance_state_effect(
                        appearance,
                        HandsetCallState::RemoteMultiline,
                        true,
                    ));
                }
                if let Some(device) = self.devices.get_mut(&appearance.device_id) {
                    device.selected_calls.remove(&appearance.sccp_id);
                }
            }
        }
        effects.push(
            HandsetEffect::BeginMedia {
                device_id: requester.device_id.clone(),
                call_id: requester.sccp_id,
                codec: requester.codec,
            }
            .into(),
        );
        self.select_line(&requester.device_id, requester.line_instance);
        self.set_call_selected(&requester.device_id, call_id, true);
        debug_assert!(self.invariant_error().is_none());
        effects
    }

    /// Restore every device microphone still owned by a committed one-way
    /// auto-answer before the handset server is stopped. Clearing the marker
    /// while producing the effects makes repeated shutdown/drain calls exact.
    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn drain_one_way_microphones(&mut self) -> Vec<DriverEffect> {
        let mut owned = self
            .call_registry
            .appearances
            .values()
            .filter(|appearance| appearance.auto_answer_mode == Some(AutoAnswerMode::OneWay))
            .map(|appearance| {
                (
                    appearance.device_id.clone(),
                    appearance.sccp_id,
                    appearance.id,
                )
            })
            .collect::<Vec<_>>();
        owned.sort_by_key(|(device_id, call_id, _)| (device_id.clone(), call_id.0));
        let mut effects = Vec::with_capacity(owned.len());
        for (device_id, call_id, appearance_id) in owned {
            let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) else {
                continue;
            };
            appearance.auto_answer_mode = None;
            effects.push(
                HandsetEffect::SetMicrophoneMode {
                    device_id,
                    call_id,
                    enabled: true,
                }
                .into(),
            );
        }
        effects
    }

    /// Publish RingOut only after an outbound remote-identity update has
    /// established the called party. Repeated or late identity callbacks may
    /// refresh CallInfo but cannot regress an answered call.
    pub fn pbx_remote_identity_ready(&mut self, pbx_id: PbxCallId) -> Vec<DriverEffect> {
        let Some(call) = self.call_registry.pbx.get_mut(&pbx_id) else {
            return Vec::new();
        };
        if call.direction != CallDirection::Outbound || call.state != CallState::Calling {
            return Vec::new();
        }
        if call.outbound_identity_stage != OutboundIdentityStage::RingOutPublished {
            call.outbound_identity_stage = OutboundIdentityStage::Ready;
        }
        self.publish_outbound_ring_out(pbx_id)
    }

    pub fn inbound_offers_for_pbx(&self, pbx_id: PbxCallId) -> Vec<InboundOffer> {
        self.call_registry
            .pbx
            .get(&pbx_id)
            .filter(|call| call.direction == CallDirection::Inbound)
            .into_iter()
            .flat_map(|call| call.appearance_ids.iter())
            .filter_map(|appearance_id| self.call_registry.appearances.get(appearance_id))
            .filter(|appearance| appearance.state == CallState::Ringing)
            .map(|appearance| self.inbound_offer_for_appearance(appearance))
            .collect()
    }

    fn inbound_offer(&self, candidate: &InboundAppearance) -> InboundOffer {
        let call_waiting = self.device_has_active_call(&candidate.binding.device_id);
        InboundOffer {
            device_id: candidate.binding.device_id.clone(),
            line_instance: candidate.binding.line_instance,
            call_id: candidate.call_id,
            ring_mode: candidate.binding.appearance.ring_mode,
            state: if call_waiting {
                HandsetCallState::CallWaiting
            } else {
                HandsetCallState::RingIn
            },
        }
    }

    pub fn cancel_inbound_offer(&mut self, call_id: CallId) -> bool {
        let Some(appearance) = self.appearance_for_call(call_id).cloned() else {
            return false;
        };
        let Some(call) = self.call_registry.pbx.get(&appearance.pbx_id) else {
            return false;
        };
        if self.redirect_claims.contains(&appearance.pbx_id)
            || call.state != CallState::Ringing
            || call.active_appearance.is_some()
        {
            return false;
        }
        #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
        self.pending_auto_answers.remove(&call_id);
        self.remove_appearance(appearance.id);
        if self
            .call_registry
            .pbx
            .get(&appearance.pbx_id)
            .is_some_and(|call| call.appearance_ids.is_empty())
        {
            self.call_registry.pbx.remove(&appearance.pbx_id);
        }
        debug_assert!(self.invariant_error().is_none());
        true
    }

    fn outbound_route_presentation(
        &mut self,
        pbx_id: PbxCallId,
        destination: &str,
    ) -> Vec<DriverEffect> {
        let Some(appearance_id) = self.call_registry.pbx.get(&pbx_id).and_then(|call| {
            (call.direction == CallDirection::Outbound && call.state == CallState::Calling)
                .then(|| {
                    call.active_appearance
                        .or_else(|| call.appearance_ids.first().copied())
                })
                .flatten()
        }) else {
            return Vec::new();
        };
        let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) else {
            return Vec::new();
        };
        appearance.info.called_number = destination.to_owned();
        let device_id = appearance.device_id.clone();
        let call_id = appearance.sccp_id;
        let info = appearance.info.clone();
        if let Some(call) = self.call_registry.pbx.get_mut(&pbx_id) {
            call.outbound_phase = Some(OutboundCallPhase::Routing);
        }
        self.refresh_conference_participant_identity(pbx_id);
        vec![
            HandsetEffect::CommitOutboundCall {
                device_id,
                call_id,
                info,
            }
            .into(),
        ]
    }

    fn allocate_pbx_id(&mut self) -> PbxCallId {
        loop {
            let id = self.next_pbx_id.into();
            self.next_pbx_id = self.next_pbx_id.wrapping_add(1).max(1);
            if !self.call_registry.pbx.contains_key(&id) {
                return id;
            }
        }
    }

    fn allocate_bridge_id(&mut self) -> PbxBridgeId {
        loop {
            let id = self.next_bridge_id.into();
            self.next_bridge_id = self.next_bridge_id.wrapping_add(1).max(1);
            if !self
                .barges
                .groups
                .values()
                .any(|group| group.bridge_id == id)
                && !self
                    .conferences
                    .by_consultation
                    .values()
                    .any(|conference| conference.bridge_id == id)
            {
                return id;
            }
        }
    }

    fn invariant_error(&self) -> Option<String> {
        invariants::validate(self)
    }
}

fn conference_participant_identity(
    call: &PbxCall,
    appearance: &CallAppearance,
) -> ConferenceParticipantIdentity {
    if call.privacy || appearance.privacy || appearance.info.party_restrictions != 0 {
        return ConferenceParticipantIdentity::default();
    }
    let (display_name, number) = match appearance.info.direction {
        CallDirection::Inbound => (
            &appearance.info.calling_name,
            &appearance.info.calling_number,
        ),
        CallDirection::Outbound => (&appearance.info.called_name, &appearance.info.called_number),
    };
    ConferenceParticipantIdentity {
        display_name: display_name.clone(),
        number: number.clone(),
    }
}

fn inbound_call_appearance(
    id: CallAppearanceId,
    pbx_id: PbxCallId,
    candidate: &InboundAppearance,
) -> CallAppearance {
    CallAppearance {
        id,
        sccp_id: candidate.call_id,
        pbx_id,
        device_id: candidate.binding.device_id.clone(),
        line_instance: candidate.binding.line_instance,
        state: CallState::Ringing,
        ring_mode: candidate.binding.appearance.ring_mode,
        privacy: candidate.binding.appearance.privacy,
        info: CallInfo {
            direction: CallDirection::Inbound,
            called_name: candidate.binding.appearance.display_label().to_owned(),
            called_number: candidate.binding.line.number.clone(),
            ..CallInfo::default()
        },
        codec: candidate.codec,
        audio: MediaStreamState::Closed,
        audio_transmit: MediaStreamState::Closed,
        video: VideoMediaState::default(),
        auto_answer_mode: None,
    }
}

fn shared_appearance_state(state: CallState, has_active_appearance: bool) -> CallState {
    match state {
        CallState::Ringing => CallState::Ringing,
        CallState::Held => CallState::SharedHeld,
        CallState::Connected => CallState::RemoteInUse,
        CallState::Collecting
        | CallState::PickupCollecting
        | CallState::Calling
        | CallState::Parking
        | CallState::Retrieving
        | CallState::TransferCollecting
            if has_active_appearance =>
        {
            CallState::RemoteInUse
        }
        state => state,
    }
}

fn appearance_state_effect(
    appearance: &CallAppearance,
    state: HandsetCallState,
    stop_media: bool,
) -> DriverEffect {
    HandsetEffect::SetCallState {
        device_id: appearance.device_id.clone(),
        call_id: appearance.sccp_id,
        state,
        stop_media,
    }
    .into()
}

fn appearance_terminal_effects(appearance: &CallAppearance) -> Vec<DriverEffect> {
    let mut effects = Vec::with_capacity(2);
    if appearance.auto_answer_mode == Some(AutoAnswerMode::OneWay) {
        effects.push(
            HandsetEffect::SetMicrophoneMode {
                device_id: appearance.device_id.clone(),
                call_id: appearance.sccp_id,
                enabled: true,
            }
            .into(),
        );
    }
    effects.push(appearance_state_effect(
        appearance,
        HandsetCallState::OnHook,
        true,
    ));
    effects
}

fn digit_character(digit: Digit) -> Option<char> {
    match digit {
        Digit::Number(number @ 0..=9) => Some(char::from(b'0' + number)),
        Digit::Number(_) => None,
        Digit::Star => Some('*'),
        Digit::Pound => Some('#'),
        Digit::A => Some('A'),
        Digit::B => Some('B'),
        Digit::C => Some('C'),
        Digit::D => Some('D'),
        Digit::Unknown(_) => None,
    }
}

#[cfg(test)]
#[path = "controller/tests/mod.rs"]
mod tests;
