//! Backend-neutral effect values emitted by the controller.

use super::*;

macro_rules! backend_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(pub u64);

        impl $name {
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            pub const fn get(self) -> u64 {
                self.0
            }
        }

        impl From<u64> for $name {
            fn from(value: u64) -> Self {
                Self::new(value)
            }
        }

        impl From<$name> for u64 {
            fn from(value: $name) -> Self {
                value.get()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

backend_id!(/// Driver-owned identity for a PBX channel.
    PbxCallId);

backend_id!(/// Driver-owned identity for a backend bridge, independent of PBX pointers and names.
    PbxBridgeId);

/// One configured request to run a PBX-hosted conference application for an
/// already-created outbound channel. The application name is deliberately not
/// configurable: this operation always targets the host's ConfBridge service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConferenceDestinationOperation {
    pub call_id: PbxCallId,
    pub destination: String,
    pub application_options: String,
    pub(crate) handset_call_id: CallId,
    pub(crate) held_calls: Vec<PbxCallId>,
    pub(crate) mutation: ConferenceMutationToken,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BridgeOperation {
    Create {
        bridge_id: PbxBridgeId,
    },
    Destroy {
        bridge_id: PbxBridgeId,
    },
    AddParticipant {
        bridge_id: PbxBridgeId,
        call_id: PbxCallId,
    },
    RemoveParticipant {
        bridge_id: PbxBridgeId,
        call_id: PbxCallId,
    },
    /// Atomically merge the two live call bridges that make up an attended
    /// consultation into the driver-owned conference bridge.
    MergeConsultation {
        bridge_id: PbxBridgeId,
        original_call_id: PbxCallId,
        consultation_call_id: PbxCallId,
    },
    /// Atomically merge every selected live-call bridge into one conference.
    /// The call order is policy-significant: the first call is the moderator.
    MergeCalls {
        bridge_id: PbxBridgeId,
        call_ids: Vec<PbxCallId>,
    },
    /// Merge one newly answered consultation bridge into an existing
    /// conference without disturbing its current participants.
    MergeParticipant {
        bridge_id: PbxBridgeId,
        call_id: PbxCallId,
    },
    /// Suppress or restore audio entering the conference from one participant.
    /// The bridge and channel identities are both validated by the backend.
    SetParticipantMuted {
        bridge_id: PbxBridgeId,
        participant_id: ParticipantId,
        call_id: PbxCallId,
        muted: bool,
    },
    /// Remove one non-moderator member from a live conference and queue normal
    /// clearing for its channel after validating exact bridge membership.
    RemoveConferenceParticipant {
        bridge_id: PbxBridgeId,
        participant_id: ParticipantId,
        call_id: PbxCallId,
    },
    /// Start or stop PBX-generated music for one exact live bridge member.
    SetParticipantMusicOnHold {
        bridge_id: PbxBridgeId,
        participant_id: ParticipantId,
        call_id: PbxCallId,
        class: String,
        enabled: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConferenceAnnouncement {
    Connected,
    ParticipantJoined(ParticipantId),
    ParticipantMuted(ParticipantId),
    ParticipantUnmuted(ParticipantId),
    ParticipantRemoved(ParticipantId),
    ModeratorDeparted(ParticipantId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConferenceAnnouncementTarget {
    pub participant_id: ParticipantId,
    pub call_id: PbxCallId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConferenceAnnouncementOperation {
    pub conference_id: ConferenceId,
    pub targets: Vec<ConferenceAnnouncementTarget>,
    pub announcement: ConferenceAnnouncement,
}

/// A live shared-call barge uses a separate PBX channel for the barging
/// handset while retaining the original call as the bridge anchor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BargeOperation {
    Join {
        bridge_id: PbxBridgeId,
        target_call_id: PbxCallId,
        barger_call_id: PbxCallId,
    },
    Leave {
        bridge_id: PbxBridgeId,
        barger_call_id: PbxCallId,
        last_participant: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PickupOperation {
    Group {
        call_id: PbxCallId,
        device_id: DeviceId,
        handset_call_id: CallId,
        codec: Codec,
        answer: bool,
    },
    Directed {
        call_id: PbxCallId,
        device_id: DeviceId,
        handset_call_id: CallId,
        codec: Codec,
        extension: String,
        context: String,
        answer: bool,
    },
}

impl PickupOperation {
    pub(super) fn handset(&self) -> (&DeviceId, CallId, Codec, bool) {
        match self {
            Self::Group {
                device_id,
                handset_call_id,
                codec,
                answer,
                ..
            }
            | Self::Directed {
                device_id,
                handset_call_id,
                codec,
                answer,
                ..
            } => (device_id, *handset_call_id, *codec, *answer),
        }
    }
}

/// Party information captured from the ringing channel before pickup moves it
/// onto the picking handset's PBX channel.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PickupOutcome {
    pub calling_name: String,
    pub calling_number: String,
    pub connected_name: String,
    pub connected_number: String,
    pub redirecting_name: String,
    pub redirecting_number: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParkingOperation {
    Park {
        call_id: PbxCallId,
        lot: Option<String>,
    },
    Retrieve {
        call_id: PbxCallId,
        lot: Option<String>,
        slot: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ManagementEventKind {
    Registration,
    Alarm,
    Feature,
    Media,
    Call,
}

#[derive(Clone, Eq, PartialEq)]
pub enum ManagementValue {
    Text(String),
    Signed(i64),
    Unsigned(u64),
    Boolean(bool),
    Redacted,
}

impl From<String> for ManagementValue {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&str> for ManagementValue {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

impl From<i64> for ManagementValue {
    fn from(value: i64) -> Self {
        Self::Signed(value)
    }
}

impl From<u64> for ManagementValue {
    fn from(value: u64) -> Self {
        Self::Unsigned(value)
    }
}

impl From<bool> for ManagementValue {
    fn from(value: bool) -> Self {
        Self::Boolean(value)
    }
}

impl fmt::Debug for ManagementValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(_) => formatter.write_str("Text(<redacted>)"),
            Self::Signed(value) => formatter.debug_tuple("Signed").field(value).finish(),
            Self::Unsigned(value) => formatter.debug_tuple("Unsigned").field(value).finish(),
            Self::Boolean(value) => formatter.debug_tuple("Boolean").field(value).finish(),
            Self::Redacted => formatter.write_str("Redacted"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagementField {
    pub name: String,
    pub value: ManagementValue,
}

impl ManagementField {
    pub fn new(name: impl Into<String>, value: impl Into<ManagementValue>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }

    pub fn redacted(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: ManagementValue::Redacted,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagementEvent {
    pub kind: ManagementEventKind,
    pub fields: Vec<ManagementField>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PbxEffect {
    CreateChannel {
        handset_call_id: CallId,
        call_id: PbxCallId,
        binding: Box<LineBinding>,
        codec: Codec,
    },
    CreateConsultationChannel {
        source_call_id: PbxCallId,
        handset_call_id: CallId,
        call_id: PbxCallId,
        binding: Box<LineBinding>,
        codec: Codec,
    },
    StartRouting {
        call_id: PbxCallId,
        context: String,
        destination: String,
    },
    Forward {
        operation: ForwardingOperation,
    },
    Voicemail {
        operation: VoicemailOperation,
    },
    StartConferenceDestination {
        operation: ConferenceDestinationOperation,
    },
    Answer {
        call_id: PbxCallId,
    },
    Hangup {
        call_id: PbxCallId,
    },
    SendDigit {
        call_id: PbxCallId,
        digit: char,
    },
    ConfigureMedia {
        call_id: PbxCallId,
        device_id: DeviceId,
        handset_call_id: CallId,
        codec: Codec,
        remote: MediaEndpoint,
    },
    /// Point Asterisk at the handset after an outbound hole-punch transaction.
    /// StartMediaTransmission was already sent alongside OpenReceiveChannel,
    /// so this effect deliberately has no handset follow-up.
    ConfigureMediaOnly {
        call_id: PbxCallId,
        device_id: DeviceId,
        codec: Codec,
        remote: MediaEndpoint,
    },
    Hold {
        call_id: PbxCallId,
    },
    Resume {
        call_id: PbxCallId,
    },
    Transfer {
        operation: TransferCompletion,
    },
    Bridge {
        operation: BridgeOperation,
    },
    Barge {
        operation: BargeOperation,
    },
    Pickup {
        operation: PickupOperation,
    },
    Parking {
        operation: ParkingOperation,
    },
    ConferenceAnnouncement {
        operation: ConferenceAnnouncementOperation,
    },
    PublishManagementEvent {
        event: ManagementEvent,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HandsetEffect {
    BeginCall {
        device_id: DeviceId,
        line_instance: u32,
        call_id: CallId,
        codec: Codec,
    },
    BeginTransfer {
        device_id: DeviceId,
        source_call_id: CallId,
        consultation_call_id: CallId,
        consultation_line_instance: u32,
        codec: Codec,
    },
    StartTone {
        device_id: DeviceId,
        call_id: CallId,
        tone: Tone,
    },
    CommitOutboundCall {
        device_id: DeviceId,
        call_id: CallId,
        info: CallInfo,
    },
    PresentOutboundProceeding {
        device_id: DeviceId,
        call_id: CallId,
        info: CallInfo,
    },
    PresentOutboundRinging {
        device_id: DeviceId,
        call_id: CallId,
        info: CallInfo,
    },
    SetCallInfo {
        device_id: DeviceId,
        call_id: CallId,
        info: CallInfo,
    },
    BeginMedia {
        device_id: DeviceId,
        call_id: CallId,
        codec: Codec,
    },
    /// Present provisional answer state and open receive media for a physical
    /// inbound answer. Full Connected presentation remains acknowledgement-gated.
    BeginAnswerMedia {
        device_id: DeviceId,
        call_id: CallId,
        codec: Codec,
    },
    /// Open both outbound media directions as one ordered transaction. Older
    /// 79x1 phones may not acknowledge receive media until transmit is open.
    BeginOutboundMedia {
        device_id: DeviceId,
        call_id: CallId,
        codec: Codec,
    },
    /// Open normal receive media while presenting a one-way intercom call.
    /// The microphone policy is a separate confirmed effect so partial
    /// execution can be compensated exactly.
    BeginOneWayMedia {
        device_id: DeviceId,
        call_id: CallId,
        codec: Codec,
    },
    BeginEarlyMedia {
        device_id: DeviceId,
        call_id: CallId,
        codec: Codec,
    },
    OpenVideoReceive {
        device_id: DeviceId,
        call_id: CallId,
        session_generation: SessionGeneration,
    },
    StartVideoTransmit {
        device_id: DeviceId,
        call_id: CallId,
        session_generation: SessionGeneration,
    },
    RefreshVideo {
        device_id: DeviceId,
        call_id: CallId,
        session_generation: SessionGeneration,
        passthrough_party_id: PassthroughPartyId,
    },
    StopVideo {
        device_id: DeviceId,
        call_id: CallId,
        session_generation: SessionGeneration,
    },
    StartMedia {
        device_id: DeviceId,
        call_id: CallId,
        endpoint: MediaEndpoint,
    },
    SetCallState {
        device_id: DeviceId,
        call_id: CallId,
        state: HandsetCallState,
        stop_media: bool,
    },
    SetMicrophoneMode {
        device_id: DeviceId,
        call_id: CallId,
        enabled: bool,
    },
    PickupCompleted {
        device_id: DeviceId,
        call_id: CallId,
        codec: Codec,
        answer: bool,
        parties: PickupOutcome,
    },
    ShowConferenceList {
        device_id: DeviceId,
        call_id: CallId,
        conference_id: ConferenceId,
        participants: Vec<ConferenceListEntry>,
    },
    ShowConferenceParticipantActions {
        device_id: DeviceId,
        call_id: CallId,
        conference_id: ConferenceId,
        participant: ConferenceListEntry,
        removable: bool,
        demotable: bool,
    },
}

impl HandsetEffect {
    pub fn device_id(&self) -> &DeviceId {
        match self {
            Self::BeginCall { device_id, .. }
            | Self::BeginTransfer { device_id, .. }
            | Self::StartTone { device_id, .. }
            | Self::CommitOutboundCall { device_id, .. }
            | Self::PresentOutboundProceeding { device_id, .. }
            | Self::PresentOutboundRinging { device_id, .. }
            | Self::SetCallInfo { device_id, .. }
            | Self::BeginMedia { device_id, .. }
            | Self::BeginAnswerMedia { device_id, .. }
            | Self::BeginOutboundMedia { device_id, .. }
            | Self::BeginOneWayMedia { device_id, .. }
            | Self::BeginEarlyMedia { device_id, .. }
            | Self::OpenVideoReceive { device_id, .. }
            | Self::StartVideoTransmit { device_id, .. }
            | Self::RefreshVideo { device_id, .. }
            | Self::StopVideo { device_id, .. }
            | Self::StartMedia { device_id, .. }
            | Self::SetCallState { device_id, .. }
            | Self::SetMicrophoneMode { device_id, .. }
            | Self::PickupCompleted { device_id, .. }
            | Self::ShowConferenceList { device_id, .. }
            | Self::ShowConferenceParticipantActions { device_id, .. } => device_id,
        }
    }

    /// Returns the call directly acted on by this handset effect.
    pub const fn subject_call_id(&self) -> CallId {
        match self {
            Self::BeginTransfer {
                consultation_call_id,
                ..
            } => *consultation_call_id,
            Self::BeginCall { call_id, .. }
            | Self::StartTone { call_id, .. }
            | Self::CommitOutboundCall { call_id, .. }
            | Self::PresentOutboundProceeding { call_id, .. }
            | Self::PresentOutboundRinging { call_id, .. }
            | Self::SetCallInfo { call_id, .. }
            | Self::BeginMedia { call_id, .. }
            | Self::BeginAnswerMedia { call_id, .. }
            | Self::BeginOutboundMedia { call_id, .. }
            | Self::BeginOneWayMedia { call_id, .. }
            | Self::BeginEarlyMedia { call_id, .. }
            | Self::OpenVideoReceive { call_id, .. }
            | Self::StartVideoTransmit { call_id, .. }
            | Self::RefreshVideo { call_id, .. }
            | Self::StopVideo { call_id, .. }
            | Self::StartMedia { call_id, .. }
            | Self::SetCallState { call_id, .. }
            | Self::SetMicrophoneMode { call_id, .. }
            | Self::PickupCompleted { call_id, .. }
            | Self::ShowConferenceList { call_id, .. }
            | Self::ShowConferenceParticipantActions { call_id, .. } => *call_id,
        }
    }

    /// Returns the call whose transition milestone this effect can settle.
    pub const fn transition_call_id(&self) -> Option<CallId> {
        match self {
            Self::BeginCall { call_id, .. }
            | Self::StartTone { call_id, .. }
            | Self::CommitOutboundCall { call_id, .. }
            | Self::PresentOutboundProceeding { call_id, .. }
            | Self::PresentOutboundRinging { call_id, .. }
            | Self::SetCallInfo { call_id, .. }
            | Self::BeginMedia { call_id, .. }
            | Self::BeginAnswerMedia { call_id, .. }
            | Self::BeginOutboundMedia { call_id, .. }
            | Self::BeginOneWayMedia { call_id, .. }
            | Self::BeginEarlyMedia { call_id, .. }
            | Self::OpenVideoReceive { call_id, .. }
            | Self::StartVideoTransmit { call_id, .. }
            | Self::RefreshVideo { call_id, .. }
            | Self::StopVideo { call_id, .. }
            | Self::StartMedia { call_id, .. }
            | Self::SetCallState { call_id, .. }
            | Self::SetMicrophoneMode { call_id, .. } => Some(*call_id),
            Self::BeginTransfer { .. }
            | Self::PickupCompleted { .. }
            | Self::ShowConferenceList { .. }
            | Self::ShowConferenceParticipantActions { .. } => None,
        }
    }
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ConferenceStartProgress {
    active_leg_held: bool,
    active_handset_held: bool,
    channel_created: bool,
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
impl ConferenceStartProgress {
    pub(crate) const fn active_leg_held(self) -> bool {
        self.active_leg_held
    }

    pub(crate) const fn active_handset_held(self) -> bool {
        self.active_handset_held
    }

    pub(crate) const fn channel_created(self) -> bool {
        self.channel_created
    }
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
impl From<&DriverEffect> for ConferenceStartProgress {
    fn from(effect: &DriverEffect) -> Self {
        Self {
            active_leg_held: matches!(effect, DriverEffect::Backend(PbxEffect::Hold { .. })),
            active_handset_held: matches!(
                effect,
                DriverEffect::Handset(HandsetEffect::SetCallState {
                    state: HandsetCallState::Hold,
                    ..
                })
            ),
            channel_created: matches!(
                effect,
                DriverEffect::Backend(PbxEffect::CreateChannel { .. })
            ),
        }
    }
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
impl BitOrAssign for ConferenceStartProgress {
    fn bitor_assign(&mut self, completed: Self) {
        self.active_leg_held |= completed.active_leg_held;
        self.active_handset_held |= completed.active_handset_held;
        self.channel_created |= completed.channel_created;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DriverEffect {
    Backend(PbxEffect),
    Handset(HandsetEffect),
}

impl From<PbxEffect> for DriverEffect {
    fn from(effect: PbxEffect) -> Self {
        Self::Backend(effect)
    }
}

impl From<HandsetEffect> for DriverEffect {
    fn from(effect: HandsetEffect) -> Self {
        Self::Handset(effect)
    }
}
