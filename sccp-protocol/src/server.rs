//! Stateful SCCP station-session boundary.
//!
//! A successful socket write or TCP acknowledgement proves only transport
//! delivery. Media and other correlated operations remain provisional until
//! the station sends the matching SCCP response. Every new media transaction
//! gets a fresh nonzero wire token (a deliberately coupled ORC/SMT pair shares
//! one), and the session accepts an acknowledgement only for the exact
//! direction and current request generation. The zero-party acknowledgement
//! fallback is limited to the first generation, where the
//! stable call reference still makes it unambiguous; reopened media fails
//! closed instead of letting an old acknowledgement settle a new request.
//! Deadlines retire that same generation before a late response is considered.
//! Handset presentation, receive media, transmit media, and call ownership are
//! therefore separate states rather than one broad "connected" flag.
//!
//! # Runtime workflow
//!
//! [`Server::bind`] creates a plain TCP listener, while
//! [`Server::with_ingress`] lets a transport owner inject clear or already
//! decrypted streams through [`ServerIngress`]. Both constructors return a
//! [`ServerHandle`] for commands and a bounded [`Event`] receiver for handset
//! input and session outcomes. The caller must run [`Server::run`] for any of
//! those channels to make progress and should consume events continuously so a
//! full event queue cannot apply backpressure to station sessions.
//!
//! A station becomes addressable by [`Command`] only after registration has
//! selected a configured [`DeviceDefinition`] and emitted
//! [`DeviceEventKind::Registered`]. Call adapters reserve or receive a
//! [`CallId`], send commands through the handle, and react to correlated media
//! acknowledgements delivered as device events. [`ServerHandle::send`] confirms
//! queue admission; [`ServerHandle::send_confirmed`] additionally waits for the
//! complete encoded command to reach the station stream. Neither operation
//! substitutes for a protocol acknowledgement when the command defines one.
//!
//! Reconfiguration replaces definitions atomically and disconnects only the
//! sessions named by [`ReconfigureResult`] or explicitly marked as affected.
//! [`ServerHandle::shutdown`] asks the run loop to disconnect all sessions and
//! finish. Dropping every handle also closes the command channel and causes the
//! same orderly exit.

mod qos;
mod transport;

pub use qos::{
    SignalingSocket, SocketQosFailure, SocketQosMark, SocketQosPolicy, SocketQosReport,
    StationSocketQos, apply_socket_qos,
};
pub use transport::{
    ObservationConnectionId, ServerObservation, ServerObservationKind, SignalingDirection,
    SignalingFidelity, SignalingObservation, StationDisconnectReason,
};
pub use transport::{ServerIngress, StationIo};

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::num::NonZeroU16;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as SyncMutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
#[cfg(test)]
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::message::BUTTON_TEMPLATE_ENTRIES_PER_CHUNK;
use crate::message::capabilities::StationMediaCapabilities;
use crate::message::values::{
    AlarmSeverity, BusyLampFieldState, ButtonType, CallHistoryDisposition, CallState, Codec,
    CodecKind, DeviceType, Digit, DtmfMode, EchoCancellation, EncryptionCapability, G723BitRate,
    IpAddressType, KeyMode, LampMode, MediaStatus, MicrophoneMode, MiscCommandType,
    NotificationPriority, PhoneFeatures, ProtocolVersion, ReceiveTransmit, ResetType, RingDuration,
    RingerMode, SilenceSuppression, SoftKey, SpeakerMode, StationSessionContext,
    StatisticsProcessing, Stimulus, SubscriptionCause, Tone, ToneDirection,
};
use crate::message::wire::{CodecError, FrameDecoder};
use crate::message::{
    AnnouncementEntry, AudioStreamControl, BoundedBytes, ButtonTemplateEntry,
    CALL_COUNT_RESPONSE_MAX_LINE_ENTRIES, CallCountLineData, CallCountResponse, ClientMessage,
    ConnectionStatistics, MediaEncryption, MediaEndpointAddress, MediaRequestIdentity,
    MediaRequestToken, MiscellaneousCommand, MulticastMediaReception, MulticastMediaTransmission,
    MultimediaPayload, MultimediaPayloadDirection, MultimediaStreamControl, OpenMultimediaChannel,
    ServerMessage, SignalingServerEndpoint,
    StartMultimediaTransmission as MultimediaTransmissionStart, UserDataV1Message,
    VideoFlowControl,
};
#[cfg(test)]
use crate::message::{ControlMessage, MediaCapability, XmlAlarmMessage};
use crate::phone::service::{
    PhoneServiceEvent, PhoneServiceExtendedRouting, PhoneServiceMessageKind, PhoneServicePayload,
    PhoneServiceRouting, parse_phone_service_payload,
};
#[cfg(test)]
use crate::phone::xml::{
    self as phone_xml, CiscoIpPhoneGraphicFileMenu, CiscoIpPhoneImageFile, CiscoIpPhoneInputItem,
    CiscoIpPhoneKeyItem, CiscoIpPhoneSoftKeyItem, CiscoIpPhoneStatus, CiscoIpPhoneStatusFile,
    CiscoIpPhoneTouchAreaMenuItem, PHONE_EXECUTE_MAX_ITEMS, PHONE_STATUS_BITMAP_MAX_BYTES,
    PhoneBackgroundHttpUrl, PhoneBitmapData, PhoneExecutePriority, PhoneImageUrl, PhoneInputFlags,
    PhoneInputParameterName, PhoneRingtoneUrl, PhoneTouchArea, PhoneXmlKey,
};
use crate::phone::xml::{
    CiscoIpPhoneExecute, CiscoIpPhoneExecuteItem, CiscoIpPhoneInput, CiscoIpPhoneMenu,
    CiscoIpPhoneMenuItem, CiscoIpPhoneSetBackground, CiscoIpPhoneSetBackgroundPreview,
    CiscoIpPhoneSetRingTone, CiscoIpPhoneText, ConferenceListAction, ConferenceListDocument,
    ConferenceListEntry, ConferenceMenuFamily, ConferenceParticipantActionsDocument,
    PHONE_BACKGROUND_APPLICATION_ID, PHONE_EXECUTE_MAX_BYTES, PHONE_IMAGE_MAX_BYTES,
    PHONE_INPUT_MAX_BYTES, PHONE_RINGTONE_APPLICATION_ID, PHONE_STATUS_MAX_BYTES,
    PHONE_TEXT_APPLICATION_ID, PHONE_TEXT_LEGACY_MAX_CHARS, PhoneAlarmTelemetry,
    PhoneBackgroundControlDocument, PhoneImageDocument, PhoneLocationTelemetry,
    PhoneServicePriority, PhoneStatusDocument, PhoneXmlError, parse_phone_alarm,
    parse_phone_location,
};
use crate::types::SignalingQos;
use crate::types::{
    ApplicationId, AudioProcessingPolicy, BlfCallerInfo, BlfState, ButtonDefinition, CallId,
    CallInfo, CallReference, ConferenceId, DEFAULT_AUDIO_MAX_FRAMES_PER_PACKET,
    DEFAULT_AUDIO_PACKET_MS, DeviceDefinition, DeviceId, DeviceRegistration, LineAppearance,
    LineDefinition, LineInstance, MediaEndpoint, MediaTrafficClass, ParticipantId,
    PassthroughPartyId, SessionGeneration, SoftKeyProfile, StationTransport,
    StationTransportRequirement, TransactionId,
};
use transport::AcceptedStation;
use transport::{ObservationSink, ObservedStationIo};

const EVENT_CAPACITY: usize = 1024;
const COMMAND_CAPACITY: usize = 1024;
const SESSION_COMMAND_CAPACITY: usize = 256;
const SESSION_ACCEPT_CAPACITY: usize = 128;
/// Maximum time a phone may leave an ordering-sensitive media command
/// unacknowledged before the call owner is notified and the stale correlation
/// state is retired.
pub const HANDSET_ACKNOWLEDGEMENT_TIMEOUT: Duration = Duration::from_secs(5);
const MEDIA_ROLLBACK_TIMEOUT: Duration = Duration::from_secs(1);
const SESSION_MEDIA_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
/// Bound for the writer acknowledgement used to serialize commands whose
/// resources must remain owned until their complete frame reaches the socket.
pub const ORDERING_ACKNOWLEDGEMENT_TIMEOUT: Duration = Duration::from_secs(5);
// A 79x1 normally follows the active accessory's Off event with OnHook within
// a few dozen milliseconds.  A route change instead reports the replacement
// accessory On immediately.  Keep the release pending long enough to
// distinguish those two transactions when firmware omits the final OnHook.
const MEDIA_PATH_RELEASE_GRACE: Duration = Duration::from_millis(150);
// A timeout only releases pending correlation state; statistics are never polled.
const CONNECTION_STATISTICS_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PENDING_CONNECTION_STATISTICS: usize = 32;
// Retired references prevent a late reply from binding to a replacement call.
const MAX_STATISTICS_REFERENCES_PER_SESSION: usize = 4096;
// Conservative defaults for phones that support multiple calls per line.
const DEFAULT_MAX_CALLS_PER_LINE: u16 = 4;
const DEFAULT_BUSY_TRIGGER_PER_LINE: u16 = 2;
const PARKING_APPLICATION_ID: u32 = 9090;
const REPLACEMENT_REGISTRATION_BACKOFF_SECONDS: u32 = 10;
pub const MIN_REGISTRATION_BACKOFF: Duration = Duration::from_secs(30);
pub const MAX_REGISTRATION_BACKOFF: Duration = Duration::from_secs(86_400);
/// Maximum number of parked calls rendered in one station selection menu.
///
/// Higher-level parking state may contain more calls; callers select and order
/// the bounded subset passed to [`CommandAction::ShowParkingMenu`].
pub const PARKING_MENU_MAX_ITEMS: usize = 32;

/// One selectable parked call rendered by the station parking application.
///
/// `slot` is the stable parking-space identifier returned by a subsequent
/// [`DeviceEventKind::ParkingMenuSelection`]. The caller and connected-party
/// fields are presentation text and do not participate in selection identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParkingMenuEntry {
    pub slot: u32,
    pub caller_name: String,
    pub caller_number: String,
    pub connected_name: String,
    pub connected_number: String,
}

/// Audible presentation to apply to a newly offered incoming call.
///
/// Passing `None` to an incoming-offer method presents the call silently;
/// [`IncomingRing::default`] selects an ordinary inside ring.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IncomingRing {
    pub mode: RingerMode,
    pub duration: RingDuration,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum IncomingPresentation {
    #[default]
    RingIn,
    CallWaiting,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IncomingOfferDelivery {
    Presented,
    SessionMissing,
    SessionStale {
        actual_generation: SessionGeneration,
    },
    CancelledBeforePresentation,
    WriteFailed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StationSessionTarget {
    device_id: DeviceId,
    generation: SessionGeneration,
}

impl StationSessionTarget {
    pub fn new(device_id: DeviceId, generation: SessionGeneration) -> Self {
        Self {
            device_id,
            generation,
        }
    }
}

#[derive(Debug)]
pub struct IncomingOfferReceipt(oneshot::Receiver<IncomingOfferDelivery>);

impl IncomingOfferReceipt {
    pub fn try_recv(&mut self) -> Result<Option<IncomingOfferDelivery>, ServerError> {
        match self.0.try_recv() {
            Ok(delivery) => Ok(Some(delivery)),
            Err(oneshot::error::TryRecvError::Empty) => Ok(None),
            Err(oneshot::error::TryRecvError::Closed) => Err(ServerError::Stopped),
        }
    }

    pub async fn wait(self) -> Result<IncomingOfferDelivery, ServerError> {
        self.0.await.map_err(|_| ServerError::Stopped)
    }
}

impl IncomingPresentation {
    const fn call_state(self) -> CallState {
        match self {
            Self::RingIn => CallState::RingIn,
            Self::CallWaiting => CallState::CallWaiting,
        }
    }
}

/// Device-wide do-not-disturb state rendered by configured feature buttons.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DoNotDisturbMode {
    #[default]
    Off,
    Silent,
    Reject,
}

/// Behavior selected for one do-not-disturb feature button.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DoNotDisturbButtonMode {
    #[default]
    Cycle,
    Silent,
    Reject,
}

/// Semantic state shared by every recording button configured on a device.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RecordingButtonState {
    #[default]
    Off,
    Armed,
    Active,
    ArmedActive,
}

impl RecordingButtonState {
    const fn is_armed(self) -> bool {
        matches!(self, Self::Armed | Self::ArmedActive)
    }

    const fn is_active(self) -> bool {
        matches!(self, Self::Active | Self::ArmedActive)
    }
}

impl Default for IncomingRing {
    fn default() -> Self {
        Self {
            mode: RingerMode::Inside,
            duration: RingDuration::Normal,
        }
    }
}

/// One fully correlated, privacy-safe station media snapshot retained independently of call
/// teardown. The firmware-specific quality payload remains opaque and is represented only by its
/// bounded byte count.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaStatisticsSnapshot {
    /// Monotonic generation assigned to the statistics request.
    ///
    /// A response is retained only when it matches the live request generation,
    /// preventing a delayed response from replacing newer statistics.
    pub request_generation: u64,
    pub call_id: CallId,
    pub line_instance: LineInstance,
    pub codec: Codec,
    pub packet_ms: u32,
    pub max_frames_per_packet: u32,
    pub receive_peer: Option<MediaEndpoint>,
    pub transmit_peer: Option<MediaEndpoint>,
    pub packets_sent: u32,
    pub octets_sent: u32,
    pub packets_received: u32,
    pub octets_received: u32,
    pub packets_lost: u32,
    pub jitter_millis: u32,
    pub latency_millis: u32,
    /// Length of the bounded opaque quality report; its contents are not
    /// retained in management state.
    pub quality_byte_count: usize,
}

/// Device-wide status-line mutation.
///
/// The optional priority selects a protocol-specific notification plane. A
/// clear with a priority removes only that plane; an unqualified clear removes
/// the ordinary status message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HandsetStatusMessage {
    Display {
        text: String,
        /// Zero keeps the message until another status mutation replaces it.
        timeout_seconds: u8,
        priority: Option<NotificationPriority>,
    },
    Clear {
        priority: Option<NotificationPriority>,
    },
}

/// Immutable policy shared by every session owned by one [`Server`].
///
/// Station definitions are supplied separately to the constructor and may be
/// replaced at runtime through [`ServerHandle::reconfigure`].
#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    /// Baseline marking applied before a station identifies itself. A device
    /// definition may replace it for the remainder of that session.
    pub signaling_qos: SignalingQos,
    /// Configured fallback used only if the accepted socket does not have a
    /// concrete local address. Normal server-list replies use the local
    /// interface selected by the operating system for that connection.
    pub advertised_address: Ipv4Addr,
    /// IPv6 fallback for an unspecified accepted local socket.
    pub advertised_ipv6_address: Option<Ipv6Addr>,
    pub server_name: String,
    /// Keepalive interval advertised to stations; session expiry uses a bounded
    /// multiple of this interval.
    pub keepalive_seconds: u32,
    /// Keepalive interval advertised for sessions using a secondary server.
    pub secondary_keepalive_seconds: u32,
    /// Ordered failover endpoints. An empty list advertises only the endpoint
    /// that accepted the current connection.
    pub signaling_servers: Vec<SignalingServerRoute>,
    /// Admission and retry policy for pre-registration token probes.
    pub registration_tokens: RegistrationTokenPolicy,
    pub firmware_version: String,
    pub dial_terminator: Digit,
    pub record_dial_terminator: bool,
    pub call_answer_order: CallSelectionOrder,
    /// Fixed station wall-clock offset from UTC. SCCP does not carry a named
    /// timezone or daylight-saving transition table.
    pub timezone_offset_minutes: i16,
    pub date_template: crate::types::DateTemplate,
    /// Optional policy-neutral station template for an otherwise unknown
    /// guest-hotline registration. The dial destination remains owned by the
    /// channel adapter and is never exposed in the handset definition.
    pub anonymous_hotline: Option<AnonymousHotlineDefinition>,
}

/// One configured server and the ports available for each signaling transport.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SignalingServerRoute {
    pub priority: u8,
    pub name: String,
    pub address: IpAddr,
    pub clear_port: Option<NonZeroU16>,
    pub secure_port: Option<NonZeroU16>,
}

impl SignalingServerRoute {
    fn endpoint(&self, transport: StationTransport) -> Option<SignalingServerEndpoint> {
        let port = match transport {
            StationTransport::Clear => self.clear_port,
            StationTransport::Secure => self.secure_port,
        }?;
        Some(SignalingServerEndpoint {
            name: self.name.clone(),
            address: self.address,
            port,
        })
    }
}

/// Decision applied to a pre-registration token probe after station and
/// transport eligibility have been established.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RegistrationFallback {
    #[default]
    Reject,
    ReturnToPrimary,
    DeviceIdOdd,
    DeviceIdEven,
}

/// Policy applied when a station probes this server while registered elsewhere.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistrationTokenPolicy {
    pub fallback: RegistrationFallback,
    pub backoff: Duration,
    pub server_priority: u8,
}

impl Default for RegistrationTokenPolicy {
    fn default() -> Self {
        Self {
            fallback: RegistrationFallback::Reject,
            backoff: Duration::from_secs(60),
            server_priority: 1,
        }
    }
}

impl RegistrationTokenPolicy {
    fn accepts(&self, device_id: &DeviceId) -> bool {
        let last_nibble = device_id
            .as_str()
            .strip_prefix("SEP")
            .filter(|mac| mac.len() == 12 && mac.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .and_then(|mac| mac.as_bytes().last().copied())
            .and_then(|byte| char::from(byte).to_digit(16));
        match self.fallback {
            RegistrationFallback::Reject => false,
            RegistrationFallback::ReturnToPrimary => self.server_priority == 1,
            RegistrationFallback::DeviceIdOdd => last_nibble.is_some_and(|value| value % 2 == 1),
            RegistrationFallback::DeviceIdEven => last_nibble.is_some_and(|value| value % 2 == 0),
        }
    }
}

/// Restricted definition used to admit an otherwise unknown station as a
/// single-line hotline device.
///
/// The server supplies only station-visible policy. Routing and authorization
/// of the resulting off-hook event remain the adapter's responsibility.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnonymousHotlineDefinition {
    label: String,
}

impl AnonymousHotlineDefinition {
    /// Build a hotline template with a validated station-visible label.
    ///
    /// Labels must contain 1 through 79 bytes and no control characters.
    pub fn new(label: impl Into<String>) -> Result<Self, ServerError> {
        let label = label.into();
        if label.is_empty() || label.len() > 79 || label.chars().any(char::is_control) {
            return Err(ServerError::InvalidConfig(
                "anonymous-hotline label must contain 1..=79 non-control bytes".into(),
            ));
        }
        Ok(Self { label })
    }

    fn device_definition(&self, id: DeviceId) -> DeviceDefinition {
        let soft_keys = SoftKeyProfile::new(KeyMode::ALL_KNOWN.iter().copied().map(|mode| {
            let actions = match mode {
                KeyMode::OnHook => vec![SoftKey::NewCall],
                KeyMode::OffHook | KeyMode::RingOut => vec![SoftKey::EndCall],
                _ => Vec::new(),
            };
            (mode, actions)
        }))
        .expect("minimal anonymous-hotline soft keys are valid");
        DeviceDefinition {
            id,
            description: self.label.clone(),
            transport: StationTransportRequirement::Either,
            signaling_qos: None,
            buttons: vec![ButtonDefinition::Line(LineAppearance::new(
                1,
                LineDefinition {
                    number: "hotline".into(),
                    display_name: self.label.clone(),
                },
            ))],
            soft_keys,
            ui: Default::default(),
        }
    }
}

/// Ordering used when a station asks to answer without identifying a call.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CallSelectionOrder {
    #[default]
    OldestFirst,
    LastFirst,
}

/// Network and framing policy for one station multicast audio direction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MulticastMediaRoute {
    pub address: IpAddr,
    pub port: u16,
    pub codec: Codec,
    pub packet_millis: u32,
}

/// Complete application-owned description of one station video receive flow.
///
/// Session-owned call, line, and request identities are deliberately absent.
/// The opaque payload is accepted only when retained from a decoded receive
/// message for the live session's protocol version.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MultimediaReceiveDescriptor {
    pub conference_id: ConferenceId,
    pub payload: MultimediaPayload,
    pub conference_creator: bool,
    pub encryption: Option<MediaEncryption>,
    pub stream_passthrough_id: u32,
    pub associated_stream_id: u32,
    pub source: MediaEndpointAddress,
    pub requested_address_type: IpAddressType,
}

impl MultimediaReceiveDescriptor {
    /// Rejects descriptors whose typed envelope cannot represent a video
    /// receive flow. Session-specific protocol and station capabilities are
    /// checked when the command is dispatched.
    pub fn validate(self) -> Result<Self, ServerError> {
        validate_multimedia_receive_descriptor(&self)?;
        Ok(self)
    }
}

/// Complete application-owned description of one station video transmit flow.
///
/// The opaque payload is accepted only when retained from a decoded transmit
/// message for the live session's protocol version. Call and request identities
/// remain session-owned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MultimediaTransmitDescriptor {
    pub conference_id: ConferenceId,
    pub endpoint: MediaEndpointAddress,
    pub payload: MultimediaPayload,
    /// Full traffic-class octet; configuration DSCP is shifted left by two.
    pub traffic_class: MediaTrafficClass,
    pub encryption: Option<MediaEncryption>,
    pub stream_passthrough_id: u32,
    pub associated_stream_id: u32,
}

impl MultimediaTransmitDescriptor {
    /// Rejects descriptors whose typed envelope cannot represent a video
    /// transmit flow. Live session policy is checked during dispatch.
    pub fn validate(self) -> Result<Self, ServerError> {
        validate_multimedia_transmit_descriptor(&self)?;
        Ok(self)
    }
}

/// Parameters for one command applied to an exact live station video encoder.
///
/// The server derives the wire command selector and fixed parameter area from
/// these variants. Arbitrary parameter bytes are intentionally unavailable at
/// this stateful command boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MultimediaTransmitControl {
    FreezePicture,
    FastPictureUpdate {
        first_gob: u32,
        gob_count: u32,
    },
    FastGobUpdate {
        first_gob: u32,
        gob_count: u32,
    },
    FastMacroblockUpdate {
        first_gob: u32,
        first_macroblock: u32,
        macroblock_count: u32,
    },
    LostPicture {
        picture_number: u32,
        long_term_picture_index: u32,
    },
    LostPartialPicture {
        picture_number: u32,
        long_term_picture_index: u32,
        first_macroblock: u32,
        macroblock_count: u32,
    },
    /// Requests recovery from at most four prior picture references.
    RecoveryReferencePicture {
        pictures: VideoPictureReferences,
    },
    TemporalSpatialTradeoff {
        value: u32,
    },
}

/// Picture identity carried by recovery feedback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoPictureReference {
    pub picture_number: u32,
    pub long_term_picture_index: u32,
}

/// A recovery request's bounded ordered picture-reference list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoPictureReferences(Box<[VideoPictureReference]>);

impl VideoPictureReferences {
    /// Collects at most five items before rejecting a list beyond the wire
    /// capacity, so even an unbounded iterator cannot cause unbounded storage.
    pub fn new(
        pictures: impl IntoIterator<Item = VideoPictureReference>,
    ) -> Result<Self, ServerError> {
        let pictures = pictures.into_iter().take(5).collect::<Vec<_>>();
        if pictures.len() > 4 {
            return Err(ServerError::InvalidMultimediaTransmitControl(
                "recovery picture count exceeds four",
            ));
        }
        Ok(Self(pictures.into_boxed_slice()))
    }

    /// Returns picture identities in wire order.
    pub fn as_slice(&self) -> &[VideoPictureReference] {
        &self.0
    }
}

impl TryFrom<Vec<VideoPictureReference>> for VideoPictureReferences {
    type Error = ServerError;

    fn try_from(pictures: Vec<VideoPictureReference>) -> Result<Self, Self::Error> {
        Self::new(pictures)
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([0, 0, 0, 0], 2000)),
            signaling_qos: SignalingQos::default(),
            advertised_address: Ipv4Addr::LOCALHOST,
            advertised_ipv6_address: None,
            server_name: "sccp-protocol".to_string(),
            keepalive_seconds: 30,
            secondary_keepalive_seconds: 30,
            signaling_servers: Vec::new(),
            registration_tokens: RegistrationTokenPolicy::default(),
            firmware_version: String::new(),
            dial_terminator: Digit::Pound,
            record_dial_terminator: false,
            call_answer_order: CallSelectionOrder::OldestFirst,
            timezone_offset_minutes: 0,
            date_template: Default::default(),
            anonymous_hotline: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Output emitted by the running server to its integration adapter.
///
/// The receiver returned by a server constructor is the sole event stream for
/// every accepted connection. Consumers should drain it continuously and use
/// the device ID carried by [`Event::Device`] rather than inferring ownership
/// from event order. They should also retain the generation from the latest
/// registration and reject later-delivered events from replaced sessions.
pub enum Event {
    SessionError {
        peer: SocketAddr,
        error: String,
    },
    /// A malformed non-registration message was discarded while the session
    /// remained usable.
    ProtocolWarning {
        peer: SocketAddr,
        /// Registered device identity, or `None` before registration.
        device_id: Option<DeviceId>,
        message_id: u32,
        error: String,
    },
    Device(DeviceEvent),
}

impl Event {
    pub fn device(
        device_id: DeviceId,
        session_generation: SessionGeneration,
        event: DeviceEventKind,
    ) -> Self {
        Self::Device(DeviceEvent::new(device_id, session_generation, event))
    }
}

/// One station-scoped item in the server event stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceEvent {
    pub device_id: DeviceId,
    /// Identifies the connection that produced this event across reconnects.
    pub session_generation: SessionGeneration,
    pub event: DeviceEventKind,
}

impl DeviceEvent {
    pub fn new(
        device_id: DeviceId,
        session_generation: SessionGeneration,
        event: DeviceEventKind,
    ) -> Self {
        Self {
            device_id,
            session_generation,
            event,
        }
    }
}

/// State transitions and handset input produced by one station session.
///
/// Call-bearing variants use the server-owned [`CallId`] rather than the raw
/// wire reference. Media success and failure variants are emitted only after
/// correlation against the current request generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeviceEventKind {
    /// Reports a station that completed registration and session initialization.
    /// Carries the accepted device definition, peer details, and negotiated state.
    Registered(DeviceRegistration),
    /// Reports that the station session ended or was replaced by a newer session.
    /// Consumers should retire state associated with this event's generation.
    Disconnected {},
    /// Replaces the station's advertised audio and multimedia capabilities.
    /// The snapshot is scoped to the connection generation that reported it.
    Capabilities {
        capabilities: StationMediaCapabilities,
    },
    /// Reports that the handset went off hook on a resolved line and call.
    /// The server has already allocated or selected the addressable call identity.
    OffHook {
        call_id: CallId,
        line_instance: LineInstance,
    },
    /// Reports that the handset went on hook for a resolved line and call.
    /// Integrations can use it to request hangup or complete hook-driven actions.
    OnHook {
        call_id: CallId,
        line_instance: LineInstance,
    },
    /// Reports one keypad digit associated with an addressable handset call.
    /// The digit is decoded but no dialing or in-call policy is applied here.
    Digit { call_id: CallId, digit: Digit },
    /// Reports a complete en-bloc number submitted for a newly allocated call.
    /// The line and server-owned call identity are resolved before emission.
    EnblocCall {
        call_id: CallId,
        line_instance: LineInstance,
        number: String,
    },
    /// Reports activation of a configured speed dial on a resolved call and line.
    /// Indicates whether the configuration permits collecting additional digits.
    SpeedDial {
        call_id: CallId,
        line_instance: LineInstance,
        number: String,
        await_further_digits: bool,
    },
    /// Reports a decoded soft-key press with its resolved station context.
    /// Call identity is present only when the selected key is call-scoped.
    SoftKey {
        /// Active call when the key is call-scoped.
        call_id: Option<CallId>,
        line_instance: LineInstance,
        soft_key: SoftKey,
    },
    /// Reports a physical line-button press and any call selected on that line.
    /// The line is always resolved even when no addressable call is active.
    LineButton {
        line_instance: LineInstance,
        call_id: Option<CallId>,
    },
    /// Reports a handset hook-flash gesture on the resolved line context.
    /// Call identity is included when the gesture belongs to an active call.
    HookFlash {
        call_id: Option<CallId>,
        line_instance: LineInstance,
    },
    /// Reports activation of a configured generic feature button.
    /// The instance identifies the exact feature definition on the station.
    FeatureButton { instance: LineInstance },
    /// Reports activation of a configured do-not-disturb feature button.
    /// The instance identifies which station feature definition was pressed.
    DoNotDisturbButton { instance: LineInstance },
    /// Reports activation of a configured recording feature button.
    /// The instance identifies the exact physical button that was pressed.
    RecordingButton { instance: LineInstance },
    /// Reports activation of a configured mobility feature button.
    /// The instance identifies the owner of any temporary mobility appearance.
    MobilityButton { instance: LineInstance },
    /// Reports a configured voicemail-button press on an exact line and call.
    /// The server allocates or resolves the addressable call before emission.
    VoicemailButton {
        call_id: CallId,
        line_instance: LineInstance,
    },
    /// Reports activation of a configured parking-lot feature button.
    /// Includes its feature instance, line context, and active call when present.
    ParkingLotButton {
        /// Feature-button instance, distinct from the call line instance.
        instance: LineInstance,
        /// Active call associated with the press, when any.
        call_id: Option<CallId>,
        line_instance: LineInstance,
    },
    /// Reports a parking slot selected from the station parking menu.
    /// Carries the configured lot identity and numeric slot chosen by the user.
    ParkingMenuSelection { lot: String, slot: u32 },
    /// Reports a correlated response submitted by a station phone service.
    /// The typed response retains application routing and submitted form values.
    PhoneServiceResponse { response: PhoneServiceEvent },
    /// Reports an action selected from a station conference service document.
    /// The typed action identifies its conference, participant, and operation.
    ConferenceListAction { action: ConferenceListAction },
    /// Reports that the current audio receive request was acknowledged.
    /// Carries the correlated call, media status, and allocated station endpoint.
    ReceiveChannelOpened {
        call_id: CallId,
        status: MediaStatus,
        endpoint: MediaEndpoint,
    },
    /// Reports that the current multimedia receive channel opened successfully.
    /// Carries the correlated call, codec, endpoint, and passthrough identity.
    MultimediaReceiveChannelOpened {
        call_id: CallId,
        codec: Codec,
        endpoint: MediaEndpointAddress,
        passthrough_party_id: PassthroughPartyId,
    },
    /// Reports a negative acknowledgement for the current multimedia receive request.
    /// Carries the failed endpoint, codec, status, and correlated passthrough identity.
    MultimediaReceiveChannelFailed {
        call_id: CallId,
        codec: Codec,
        status: MediaStatus,
        endpoint: MediaEndpointAddress,
        passthrough_party_id: PassthroughPartyId,
    },
    /// Reports expiry of the current multimedia receive acknowledgement deadline.
    /// Carries the call, codec, and passthrough identity of the retired request.
    MultimediaReceiveChannelTimedOut {
        call_id: CallId,
        codec: Codec,
        passthrough_party_id: PassthroughPartyId,
    },
    /// Reports that the current multimedia transmit request started successfully.
    /// Carries the correlated call, codec, endpoint, and passthrough identity.
    MultimediaTransmitStarted {
        call_id: CallId,
        codec: Codec,
        endpoint: MediaEndpointAddress,
        passthrough_party_id: PassthroughPartyId,
    },
    /// Reports a negative acknowledgement for the current multimedia transmit request.
    /// Carries the failed endpoint, codec, status, and correlated passthrough identity.
    MultimediaTransmitFailed {
        call_id: CallId,
        codec: Codec,
        status: MediaStatus,
        endpoint: MediaEndpointAddress,
        passthrough_party_id: PassthroughPartyId,
    },
    /// Reports expiry of the current multimedia transmit acknowledgement deadline.
    /// Carries the call, codec, and passthrough identity of the retired request.
    MultimediaTransmitTimedOut {
        call_id: CallId,
        codec: Codec,
        passthrough_party_id: PassthroughPartyId,
    },
    TransmitChannelOpen {
        call_id: CallId,
        outcome: TransmitOpenOutcome,
        endpoint: MediaEndpoint,
    },
    /// Reports expiry of an audio receive or transmit acknowledgement deadline.
    /// Identifies the correlated call and the acknowledgement type that expired.
    HandsetAcknowledgementTimedOut {
        call_id: CallId,
        acknowledgement: HandsetAcknowledgement,
    },
    /// Reports a station-declared failure of an active audio transmission.
    /// Carries the correlated call, media status, and failed remote endpoint.
    MediaTransmissionFailed {
        call_id: CallId,
        status: MediaStatus,
        endpoint: MediaEndpoint,
    },
    /// Reports that the current multicast receive request started successfully.
    /// Carries the exact conference, call, and admitted multicast route.
    MulticastReceptionStarted {
        conference_id: ConferenceId,
        call_id: CallId,
        route: MulticastMediaRoute,
    },
    /// Reports a negative acknowledgement for the current multicast receive request.
    /// Carries the exact conference, call, and station-reported failure status.
    MulticastReceptionFailed {
        conference_id: ConferenceId,
        call_id: CallId,
        status: MediaStatus,
    },
    /// Reports expiry of the current multicast receive acknowledgement deadline.
    /// Identifies the exact conference and call whose request was retired.
    MulticastReceptionTimedOut {
        conference_id: ConferenceId,
        call_id: CallId,
    },
    /// Reports that multicast transmission began for the requested route.
    /// Carries the exact conference, call, and admitted multicast destination.
    MulticastTransmissionStarted {
        conference_id: ConferenceId,
        call_id: CallId,
        route: MulticastMediaRoute,
    },
    /// Reports a station-declared failure of multicast audio transmission.
    /// Carries the conference, call, status, and failed multicast endpoint.
    MulticastTransmissionFailed {
        conference_id: ConferenceId,
        call_id: CallId,
        status: MediaStatus,
        address: IpAddr,
        port: u16,
    },
    /// Reports a correlated media-statistics response retained by the server.
    /// Carries the complete snapshot for the exact call and media generation.
    ConnectionStatisticsCollected { snapshot: MediaStatisticsSnapshot },
    /// Reports a decoded fixed-layout station alarm and optional parameters.
    /// Carries the station-provided severity and bounded human-readable text.
    Alarm {
        severity: AlarmSeverity,
        text: String,
        parameters: Option<[u32; 2]>,
    },
    /// Reports a typed alarm decoded from a station XML telemetry document.
    /// The parsed telemetry exposes only the validated alarm schema and values.
    XmlAlarm { telemetry: PhoneAlarmTelemetry },
    /// Reports typed station location data decoded from an XML document.
    /// The telemetry retains validated address and location fields in wire order.
    LocationInformation { telemetry: PhoneLocationTelemetry },
    /// Reports that the station headset-enabled state changed.
    /// Carries the newly reported boolean state for integration-side handling.
    HeadsetStatusChanged { enabled: bool },
    /// Reports a state transition on a station media path or accessory.
    /// Carries the typed path identity and the event reported for that path.
    MediaPathChanged {
        path: crate::message::values::MediaPathId,
        event: crate::message::values::MediaPathEvent,
    },
    /// Reports a valid client message with no implemented server-side behavior.
    /// Preserves the decoded message so integrations can observe or log it.
    UnhandledMessage { message: ClientMessage },
}

/// Station acknowledgement whose correlation deadline expired.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HandsetAcknowledgement {
    OpenReceiveChannel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransmitOpenOutcome {
    Acknowledged,
    Implied,
    NotReported,
    Rejected(MediaStatus),
}

/// Station presentation coupled to an audio receive-channel request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReceiveChannelPurpose {
    /// Opens receive media without changing the call presentation.
    Media,
    /// Presents provisional answer state immediately before opening receive media.
    InboundAnswer,
}

/// One station-targeted operation submitted through [`ServerHandle`].
///
/// Construction does not consult live session state; target and call
/// availability are checked when the running server dispatches the action.
#[derive(Clone, Debug)]
pub struct Command {
    pub device_id: DeviceId,
    pub action: CommandAction,
}

impl Command {
    pub fn new(device_id: DeviceId, action: CommandAction) -> Self {
        Self { device_id, action }
    }
}

/// Operation applied to the target station session in command-queue order.
///
/// Call-scoped actions resolve the server-owned [`CallId`] to the current wire
/// reference inside the session. Actions for stale calls are ignored, except
/// that [`Self::CloseCall`] also records an early cancellation so it can retire
/// an incoming offer that has not reached the session yet.
///
/// Outbound presentation normally progresses from [`Self::BeginCall`] through
/// proceeding or ringing to [`Self::CommitOutboundCall`]. Media actions allocate
/// a fresh request identity for each generation; close and stop actions retire
/// that identity so a late acknowledgement cannot settle replacement media.
#[derive(Clone, Debug)]
pub enum CommandAction {
    /// Creates an outbound call in the station session and makes it active.
    /// Presents the line off hook using the supplied codec and call identity.
    BeginCall {
        line_instance: LineInstance,
        call_id: CallId,
        codec: Codec,
    },
    /// Moves a held call into transfer mode and creates a consultation call.
    /// Makes the consultation call active with transfer-specific station UI.
    BeginTransfer {
        source_call_id: CallId,
        consultation_line_instance: LineInstance,
        consultation_call_id: CallId,
        codec: Codec,
    },
    /// Replaces the calling and called-party information shown for a call.
    /// Also updates the directory number used for later media statistics.
    SetCallInfo { call_id: CallId, info: CallInfo },
    /// Commits collected outbound digits and advances the call to proceeding.
    /// Updates call information, dialed-number history, lamps, and station UI.
    CommitOutboundCall { call_id: CallId, info: CallInfo },
    /// Presents an outbound call as accepted and currently proceeding.
    /// Stops dial tone and refreshes call information and the station prompt.
    PresentOutboundProceeding { call_id: CallId, info: CallInfo },
    /// Presents remote alerting for an outbound call on the station.
    /// Updates call information, prompt, soft keys, and local ringback tone.
    PresentOutboundRinging { call_id: CallId, info: CallInfo },
    /// Changes the protocol-visible state of an existing station call.
    /// Reconciles active-call ownership, history, lamps, prompts, and soft keys.
    SetCallState { call_id: CallId, state: CallState },
    /// Marks or unmarks a call in the station's selected-call set.
    /// Emits the selection status for the call's current wire reference.
    SetCallSelected { call_id: CallId, selected: bool },
    /// Displays call-scoped prompt text for an optional number of seconds.
    /// Resolves the server call identity to the station line and wire call.
    DisplayPrompt {
        call_id: CallId,
        timeout_seconds: u32,
        text: String,
    },
    /// Clears the prompt currently associated with a station call.
    /// Targets the call's resolved line instance and current wire reference.
    ClearPrompt { call_id: CallId },
    /// Replaces the station's persistent status notification and beep policy.
    /// Selects the appropriate static or dynamic display frames for the phone.
    SetStatusMessage {
        message: HandsetStatusMessage,
        beep: bool,
    },
    /// Enables or disables the station microphone outside a media renegotiation.
    /// Sends the handset microphone-mode command in command-queue order.
    SetMicrophoneMode { enabled: bool },
    /// Shows or clears the recording indicator for a particular call.
    /// Resolves the call before emitting the station recording-status frame.
    SetRecordingStatus { call_id: CallId, active: bool },
    /// Requests that the station reset using the selected reset behavior.
    /// The reset type controls whether the device restarts or resets its state.
    ResetDevice { reset_type: ResetType },
    /// Sets the message-waiting state for one configured line appearance.
    /// Caches the state and updates the corresponding station lamp immediately.
    SetMwi {
        line_instance: LineInstance,
        enabled: bool,
    },
    /// Replaces all three forwarding destinations retained for one line.
    /// A missing destination clears that forwarding kind and its display.
    SetForwardStatus {
        line_instance: LineInstance,
        forward_all: Option<String>,
        forward_busy: Option<String>,
        forward_no_answer: Option<String>,
    },
    /// Sets the enabled state of a configured generic feature button.
    /// Caches and emits the station-specific feature projection when present.
    SetFeatureStatus {
        instance: LineInstance,
        enabled: bool,
    },
    /// Updates a configured do-not-disturb button and its current mode.
    /// Projects the configured button behavior into the station UI and cache.
    SetDoNotDisturbStatus {
        instance: LineInstance,
        mode: DoNotDisturbMode,
        button_mode: DoNotDisturbButtonMode,
    },
    /// Mirrors one semantic recording state to every recording button.
    SetRecordingButtonStatus { state: RecordingButtonState },
    /// Installs or removes the temporary line owned by a mobility button.
    /// Rebuilds the button template and line status as one validated change.
    SetMobilityAppearance {
        mobility_instance: LineInstance,
        appearance: Option<LineAppearance>,
    },
    /// Updates a configured busy-lamp-field button and optional caller details.
    /// Refreshes its cached feature state and any hinted-ringing notification.
    SetBlfStatus {
        instance: LineInstance,
        state: BlfState,
        caller: Option<BlfCallerInfo>,
    },
    /// Displays a parking-lot menu containing the supplied parked calls.
    /// Records its transaction so a later station selection can be correlated.
    ShowParkingMenu {
        instance: LineInstance,
        transaction_id: TransactionId,
        lot: String,
        calls: Vec<ParkingMenuEntry>,
    },
    /// Displays the participant list for a conference attached to a call.
    /// Selects the phone-compatible menu family and sends its service document.
    ShowConferenceList {
        call_id: CallId,
        conference_id: ConferenceId,
        participants: Vec<ConferenceListEntry>,
    },
    /// Displays the allowed actions for one conference participant.
    /// Builds removal and demotion choices for the target call and phone family.
    ShowConferenceParticipantActions {
        call_id: CallId,
        conference_id: ConferenceId,
        participant: ConferenceListEntry,
        removable: bool,
        demotable: bool,
    },
    /// Displays a text-service document in the requested application envelope.
    /// Segments and encodes it according to the negotiated station protocol.
    ShowTextService {
        line_instance: LineInstance,
        call_reference: CallReference,
        transaction_id: TransactionId,
        priority: PhoneServicePriority,
        document: CiscoIpPhoneText,
    },
    /// Displays an input form and establishes its response correlation fields.
    /// A submitted form returns as a phone-service response device event.
    ShowInputService {
        line_instance: LineInstance,
        call_reference: CallReference,
        application_id: ApplicationId,
        transaction_id: TransactionId,
        priority: PhoneServicePriority,
        document: CiscoIpPhoneInput,
    },
    /// Sends a document instructing the station to execute phone actions.
    /// Preserves application routing, transaction identity, and display priority.
    ExecutePhoneActions {
        line_instance: LineInstance,
        call_reference: CallReference,
        application_id: ApplicationId,
        transaction_id: TransactionId,
        priority: PhoneServicePriority,
        document: CiscoIpPhoneExecute,
    },
    /// Displays an image-service document using the target application envelope.
    /// Encodes and segments the image for the negotiated station protocol.
    ShowImageService {
        line_instance: LineInstance,
        call_reference: CallReference,
        application_id: ApplicationId,
        transaction_id: TransactionId,
        priority: PhoneServicePriority,
        document: PhoneImageDocument,
    },
    /// Displays a status-service document with its application routing fields.
    /// Encodes and segments status content for the negotiated station protocol.
    ShowStatusService {
        line_instance: LineInstance,
        call_reference: CallReference,
        application_id: ApplicationId,
        transaction_id: TransactionId,
        priority: PhoneServicePriority,
        document: PhoneStatusDocument,
    },
    /// Applies a background image described by the supplied control document.
    /// Sends the request with the provided service transaction identity.
    SetBackgroundImage {
        transaction_id: TransactionId,
        document: CiscoIpPhoneSetBackground,
    },
    /// Previews a background image without selecting it as the active image.
    /// Sends the preview control document under the supplied transaction.
    PreviewBackgroundImage {
        transaction_id: TransactionId,
        document: CiscoIpPhoneSetBackgroundPreview,
    },
    /// Selects the station ringtone described by the supplied control document.
    /// Sends the ringtone operation under the provided service transaction.
    SetRingtone {
        transaction_id: TransactionId,
        document: CiscoIpPhoneSetRingTone,
    },
    /// Starts a user-direction tone for a call, or stops tone for `Silence`.
    /// Resolves the call to its current station line and wire reference.
    StartTone { call_id: CallId, tone: Tone },
    /// Carries a service-node request to start conference announcements.
    /// Station sessions reject it because announcement routing is not station-local.
    StartAnnouncement {
        conference_id: ConferenceId,
        announcements: Vec<AnnouncementEntry>,
        /// Marks the final request in an acknowledgement-delimited sequence.
        end_of_ack: bool,
        participant_ids: Vec<ParticipantId>,
        /// Bit mask selecting which listed participants hear the announcement.
        hearing_participant_mask: u32,
        /// Protocol playback-mode value retained for station interpretation.
        play_mode: u32,
    },
    /// Carries a service-node request to stop conference announcements.
    /// Station sessions reject it because announcement routing is not station-local.
    StopAnnouncement { conference_id: ConferenceId },
    /// Carries a conference announcement completion and playback result.
    /// Station sessions reject it because completion belongs to the service node.
    AnnouncementFinish {
        conference_id: ConferenceId,
        play_status: u32,
    },
    /// Starts audible ringing and the associated line indication for a call.
    /// Resolves the call to its current station line and wire reference.
    StartRinging { call_id: CallId },
    /// Stops audible ringing and clears the associated call indication.
    /// Leaves the call itself intact for a following state transition or close.
    StopRinging { call_id: CallId },
    /// Allocates the station's audio receive channel for a correlated request.
    /// Applies codec, packetization, DTMF, source, and processing constraints.
    OpenReceiveChannel {
        call_id: CallId,
        purpose: ReceiveChannelPurpose,
        /// Optional RTP source restriction. `None` accepts media from any
        /// source and is encoded as the SCCP wildcard endpoint `0.0.0.0:0`.
        source: Option<MediaEndpoint>,
        codec: Codec,
        packet_ms: u32,
        max_frames_per_packet: u32,
        dtmf_mode: DtmfMode,
        audio_processing: AudioProcessingPolicy,
    },
    /// Opens a video receive channel matching an advertised station capability.
    /// Replacing an existing generation closes and retires that generation first.
    OpenMultimediaReceiveChannel {
        call_id: CallId,
        descriptor: MultimediaReceiveDescriptor,
    },
    /// Closes the active multimedia receive channel for a call.
    /// Retires its request identity so late acknowledgements cannot settle it.
    CloseMultimediaReceiveChannel { call_id: CallId },
    /// Starts station video transmission using an advertised transmit capability.
    /// Replacing an existing generation stops and retires that generation first.
    StartMultimediaTransmission {
        call_id: CallId,
        descriptor: MultimediaTransmitDescriptor,
    },
    /// Stops the active multimedia transmission for a call.
    /// Retires its request identity so late acknowledgements cannot settle it.
    StopMultimediaTransmission { call_id: CallId },
    /// Sets the maximum bit rate of an exact live station video encoder.
    /// Requires the current passthrough token to reject stale stream controls.
    SetMultimediaTransmitBitRate {
        call_id: CallId,
        passthrough_party_id: PassthroughPartyId,
        maximum_bit_rate: u32,
    },
    /// Notifies the exact live station video encoder of a bit-rate change.
    /// Requires the current passthrough token to reject stale notifications.
    NotifyMultimediaTransmitBitRate {
        call_id: CallId,
        passthrough_party_id: PassthroughPartyId,
        maximum_bit_rate: u32,
    },
    /// Applies typed flow, picture, or display feedback to a video encoder.
    /// Requires the current passthrough token to target the exact live stream.
    ControlMultimediaTransmission {
        call_id: CallId,
        passthrough_party_id: PassthroughPartyId,
        control: MultimediaTransmitControl,
    },
    /// Opens both audio directions for an outbound call in one writer action.
    /// Writes receive then transmit setup without an intervening queue boundary.
    OpenOutboundMedia {
        call_id: CallId,
        source: Option<MediaEndpoint>,
        endpoint: MediaEndpoint,
        codec: Codec,
        packet_ms: u32,
        max_frames_per_packet: u32,
        dtmf_mode: DtmfMode,
        audio_processing: AudioProcessingPolicy,
        traffic_class: MediaTrafficClass,
    },
    /// Closes the station's audio receive leg for a call.
    /// Retires its pending request so a late acknowledgement is ignored.
    CloseReceiveChannel { call_id: CallId },
    /// Starts the station's audio transmit leg toward the supplied endpoint.
    /// Applies DTMF, processing, and traffic-class policy to the new generation.
    StartMedia {
        call_id: CallId,
        endpoint: MediaEndpoint,
        dtmf_mode: DtmfMode,
        audio_processing: AudioProcessingPolicy,
        traffic_class: MediaTrafficClass,
    },
    /// Starts station reception of an admitted multicast audio route.
    /// Correlates the route with the call and requested audio processing policy.
    StartMulticastReception {
        conference_id: ConferenceId,
        call_id: CallId,
        route: MulticastMediaRoute,
        echo_cancellation: EchoCancellation,
        g723_bitrate: G723BitRate,
    },
    /// Stops multicast audio reception for the exact call and conference.
    /// Retires the active receive transaction associated with that route.
    StopMulticastReception {
        conference_id: ConferenceId,
        call_id: CallId,
    },
    /// Starts station transmission onto an admitted multicast audio route.
    /// Applies precedence, silence, packetization, and G.723 rate parameters.
    StartMulticastTransmission {
        conference_id: ConferenceId,
        call_id: CallId,
        route: MulticastMediaRoute,
        precedence: u32,
        silence_suppression: SilenceSuppression,
        max_frames_per_packet: u32,
        g723_bitrate: G723BitRate,
    },
    /// Stops multicast audio transmission for the exact call and conference.
    /// Retires the active transmit transaction associated with that route.
    StopMulticastTransmission {
        conference_id: ConferenceId,
        call_id: CallId,
    },
    /// Stops the station's audio transmit leg for a call.
    /// Retires its pending request so a late acknowledgement is ignored.
    StopMedia { call_id: CallId },
    /// Tears down all station media and presentation for a call.
    /// Requests final statistics when applicable and retires the call identity.
    CloseCall { call_id: CallId },
    /// Drains all active media and terminates the target station session.
    /// Causes the running server to disconnect that device after command handling.
    DisconnectDevice {},
}

/// Failure returned by server construction, command submission, session I/O,
/// or stateful command validation.
///
/// Queue-admission failures do not imply that an earlier command failed.
/// [`Self::CommandWrite`] and [`Self::CommandAcknowledgementTimeout`] are
/// specific to [`ServerHandle::send_confirmed`]; protocol-level media outcomes
/// instead arrive through [`Event`].
#[derive(Debug, Error)]
pub enum ServerError {
    #[error("failed to bind SCCP server: {0}")]
    Bind(#[source] std::io::Error),
    #[error("SCCP server I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("SCCP protocol error: {0}")]
    Protocol(#[from] CodecError),
    #[error("invalid SCCP server configuration: {0}")]
    InvalidConfig(String),
    #[error("phone XML error: {0}")]
    PhoneXml(#[from] PhoneXmlError),
    #[error("device {0} is not connected")]
    DeviceNotConnected(DeviceId),
    #[error("call {0:?} does not exist")]
    UnknownCall(CallId),
    #[error("device {device} has no BLF feature button instance {instance}")]
    UnknownBlfButton { device: DeviceId, instance: u32 },
    #[error("call {call_id:?} cannot {operation} while in state {state:?}")]
    InvalidCallTransaction {
        call_id: CallId,
        operation: &'static str,
        state: CallState,
    },
    #[error("SCCP server has stopped")]
    Stopped,
    #[error("SCCP server command queue is full")]
    CommandQueueFull,
    #[error("SCCP command could not be written to the device: {0}")]
    CommandWrite(String),
    #[error("SCCP command writer acknowledgement timed out")]
    CommandAcknowledgementTimeout,
    #[error("SCCP station media cleanup timed out")]
    MediaCleanupTimeout,
    #[error("SCCP media request identity space is exhausted")]
    MediaRequestIdentityExhausted,
    #[error("SCCP station session generation space is exhausted")]
    SessionGenerationExhausted,
    #[error("invalid multicast media policy: {0}")]
    InvalidMulticastMedia(&'static str),
    #[error("station does not advertise the requested multicast codec")]
    UnsupportedMulticastCodec,
    #[error("invalid multimedia receive policy: {0}")]
    InvalidMultimediaReceive(&'static str),
    #[error("station does not advertise the requested video receive capability")]
    UnsupportedMultimediaReceive,
    #[error("invalid multimedia transmit policy: {0}")]
    InvalidMultimediaTransmit(&'static str),
    #[error("station does not advertise the requested video transmit capability")]
    UnsupportedMultimediaTransmit,
    #[error("invalid multimedia transmit control: {0}")]
    InvalidMultimediaTransmitControl(&'static str),
    #[error(
        "call {call_id:?} has no open multimedia transmit stream with passthrough token {passthrough_party_id}"
    )]
    StaleMultimediaTransmitControl {
        call_id: CallId,
        passthrough_party_id: PassthroughPartyId,
    },
    #[error("{message} is a control/service-node message, not a station command")]
    InvalidStationCommand { message: &'static str },
}

impl ServerError {
    const fn is_nonfatal_command_rejection(&self) -> bool {
        matches!(
            self,
            Self::InvalidCallTransaction { .. }
                | Self::UnknownBlfButton { .. }
                | Self::InvalidStationCommand { .. }
                | Self::InvalidMulticastMedia(_)
                | Self::UnsupportedMulticastCodec
                | Self::InvalidMultimediaReceive(_)
                | Self::UnsupportedMultimediaReceive
                | Self::InvalidMultimediaTransmit(_)
                | Self::UnsupportedMultimediaTransmit
                | Self::InvalidMultimediaTransmitControl(_)
                | Self::StaleMultimediaTransmitControl { .. }
        )
    }
}

/// Cloneable command and management endpoint for a running [`Server`].
///
/// The handle does not drive I/O itself: [`Server::run`] must remain active.
/// Clones share call-ID allocation, retained media statistics, and the bounded
/// command queue. Dropping the last handle closes that queue and lets the run
/// loop perform its normal session shutdown.
#[derive(Clone, Debug)]
pub struct ServerHandle {
    command_tx: mpsc::Sender<ServerCommand>,
    next_call_id: Arc<AtomicU64>,
    latest_media_statistics: Arc<RwLock<HashMap<DeviceId, MediaStatisticsSnapshot>>>,
    call_answer_order: Arc<RwLock<CallSelectionOrder>>,
}

/// The station definitions changed by one atomic server reconfiguration.
///
/// Only connected devices in `changed` or `removed` are disconnected. Added
/// devices have no session to disrupt, while definitions absent from every
/// list keep their live session and calls.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReconfigureResult {
    pub added: Vec<DeviceId>,
    pub changed: Vec<DeviceId>,
    pub removed: Vec<DeviceId>,
}

impl ReconfigureResult {
    pub fn is_unchanged(&self) -> bool {
        self.added.is_empty() && self.changed.is_empty() && self.removed.is_empty()
    }

    fn disconnected_devices(&self) -> impl Iterator<Item = &DeviceId> {
        self.changed.iter().chain(&self.removed)
    }
}

impl ServerHandle {
    /// Applies to future answer requests that omit their call reference.
    /// Explicit references and existing session calls are not rewritten.
    pub fn set_call_answer_order(&self, order: CallSelectionOrder) {
        *self
            .call_answer_order
            .write()
            .expect("SCCP call-answer-order lock poisoned") = order;
    }

    /// Return the latest fully correlated statistics response for a device.
    ///
    /// Snapshots survive call and session teardown until a newer response for
    /// that device replaces them or the server is dropped.
    pub fn latest_media_statistics(&self, device_id: &DeviceId) -> Option<MediaStatisticsSnapshot> {
        self.latest_media_statistics
            .read()
            .expect("SCCP media-statistics lock poisoned")
            .get(device_id)
            .cloned()
    }

    /// Clone every retained per-device snapshot, releasing the internal lock before a caller
    /// sorts, filters, or formats management output.
    pub fn media_statistics(&self) -> Vec<(DeviceId, MediaStatisticsSnapshot)> {
        self.latest_media_statistics
            .read()
            .expect("SCCP media-statistics lock poisoned")
            .iter()
            .map(|(device_id, snapshot)| (device_id.clone(), snapshot.clone()))
            .collect()
    }

    /// Enqueue a station command, waiting for capacity in the server queue.
    ///
    /// Success confirms queue admission only. The command may subsequently be
    /// discarded if the target session retired, and any station response is
    /// reported separately through [`Event`]. Use [`Self::send_confirmed`] when
    /// adapter resource lifetime depends on completion of the stream write.
    pub async fn send(&self, command: Command) -> Result<(), ServerError> {
        self.command_tx
            .send(ServerCommand::Public(Box::new(command)))
            .await
            .map_err(|_| ServerError::Stopped)
    }

    /// Send a command and wait until its complete encoded frame has been
    /// written to the registered device's TCP stream.
    ///
    /// This is intentionally stronger than [`Self::send`], whose completion
    /// only means the command entered the server queue. Lifecycle-sensitive
    /// callers use this boundary before releasing resources that protect the
    /// command's on-device operation.
    pub async fn send_confirmed(&self, command: Command) -> Result<(), ServerError> {
        let expires_at = Instant::now() + ORDERING_ACKNOWLEDGEMENT_TIMEOUT;
        tokio::time::timeout_at(expires_at, async {
            let (written_tx, written_rx) = oneshot::channel();
            self.command_tx
                .send(ServerCommand::Confirmed {
                    command: Box::new(command),
                    written: written_tx,
                    expires_at,
                })
                .await
                .map_err(|_| ServerError::Stopped)?;
            written_rx
                .await
                .map_err(|_| ServerError::Stopped)?
                .map_err(ServerError::CommandWrite)
        })
        .await
        .map_err(|_| ServerError::CommandAcknowledgementTimeout)?
    }

    /// Enqueue a command without yielding, preserving the ordering of
    /// synchronous channel-driver callbacks such as call followed by hangup.
    pub fn try_send(&self, command: Command) -> Result<(), ServerError> {
        self.command_tx
            .try_send(ServerCommand::Public(Box::new(command)))
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => ServerError::CommandQueueFull,
                mpsc::error::TrySendError::Closed(_) => ServerError::Stopped,
            })
    }

    /// Allocate a call ID and enqueue an ordinarily ringing incoming offer.
    ///
    /// The returned identity is stable across all later commands and handset
    /// events for the offer. A failed enqueue still consumes the reserved ID.
    pub async fn offer_incoming_call(
        &self,
        device_id: DeviceId,
        line_instance: LineInstance,
        info: CallInfo,
    ) -> Result<CallId, ServerError> {
        let call_id = self.reserve_call_id();
        self.offer_incoming_call_with_id(device_id, line_instance, call_id, info)
            .await?;
        Ok(call_id)
    }

    /// Reserve a call ID before exposing a call to a protocol session.
    ///
    /// Channel-driver adapters use this to install all private channel state
    /// before the handset can answer the subsequent offer.
    pub fn reserve_call_id(&self) -> CallId {
        CallId(self.next_call_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Offer an incoming call using an ID previously returned by
    /// [`Self::reserve_call_id`].
    pub async fn offer_incoming_call_with_id(
        &self,
        device_id: DeviceId,
        line_instance: LineInstance,
        call_id: CallId,
        info: CallInfo,
    ) -> Result<(), ServerError> {
        self.offer_incoming_call_with_id_and_ring(device_id, line_instance, call_id, info, true)
            .await
    }

    pub async fn offer_incoming_call_with_id_and_ring(
        &self,
        device_id: DeviceId,
        line_instance: LineInstance,
        call_id: CallId,
        info: CallInfo,
        audible_ring: bool,
    ) -> Result<(), ServerError> {
        self.offer_incoming_call_with_id_and_ringer(
            device_id,
            line_instance,
            call_id,
            info,
            IncomingPresentation::RingIn,
            audible_ring.then_some(IncomingRing::default()),
        )
        .await
    }

    /// Enqueue an incoming offer with explicit audible presentation.
    ///
    /// `None` creates a silent offer. `Some` applies the supplied ring mode and
    /// duration before selecting the incoming-call soft-key state.
    pub async fn offer_incoming_call_with_id_and_ringer(
        &self,
        device_id: DeviceId,
        line_instance: LineInstance,
        call_id: CallId,
        info: CallInfo,
        presentation: IncomingPresentation,
        ringer: Option<IncomingRing>,
    ) -> Result<(), ServerError> {
        self.command_tx
            .send(ServerCommand::OfferIncoming {
                device_id,
                expected_generation: None,
                line_instance,
                call_id,
                info,
                presentation,
                ringer,
                delivery: None,
            })
            .await
            .map_err(|_| ServerError::Stopped)?;
        Ok(())
    }

    /// Enqueue an incoming offer without yielding. Channel drivers should use
    /// this from their synchronous call callback so a following hangup cannot
    /// overtake the offer.
    pub fn try_offer_incoming_call_with_id(
        &self,
        device_id: DeviceId,
        line_instance: LineInstance,
        call_id: CallId,
        info: CallInfo,
    ) -> Result<(), ServerError> {
        self.try_offer_incoming_call_with_id_and_ring(device_id, line_instance, call_id, info, true)
    }

    /// Non-blocking form of [`Self::offer_incoming_call_with_id_and_ring`].
    ///
    /// Returns [`ServerError::CommandQueueFull`] without changing session state
    /// when immediate queue capacity is unavailable.
    pub fn try_offer_incoming_call_with_id_and_ring(
        &self,
        device_id: DeviceId,
        line_instance: LineInstance,
        call_id: CallId,
        info: CallInfo,
        audible_ring: bool,
    ) -> Result<(), ServerError> {
        self.try_offer_incoming_call_with_id_and_ringer(
            device_id,
            line_instance,
            call_id,
            info,
            IncomingPresentation::RingIn,
            audible_ring.then_some(IncomingRing::default()),
        )
    }

    pub fn try_offer_incoming_call_with_id_and_ringer(
        &self,
        device_id: DeviceId,
        line_instance: LineInstance,
        call_id: CallId,
        info: CallInfo,
        presentation: IncomingPresentation,
        ringer: Option<IncomingRing>,
    ) -> Result<(), ServerError> {
        self.command_tx
            .try_send(ServerCommand::OfferIncoming {
                device_id,
                expected_generation: None,
                line_instance,
                call_id,
                info,
                presentation,
                ringer,
                delivery: None,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => ServerError::CommandQueueFull,
                mpsc::error::TrySendError::Closed(_) => ServerError::Stopped,
            })
    }

    pub async fn offer_incoming_call_for_session(
        &self,
        target: StationSessionTarget,
        line_instance: LineInstance,
        call_id: CallId,
        info: CallInfo,
        presentation: IncomingPresentation,
        ringer: Option<IncomingRing>,
    ) -> Result<IncomingOfferReceipt, ServerError> {
        let (delivery, receipt) = oneshot::channel();
        let StationSessionTarget {
            device_id,
            generation,
        } = target;
        self.command_tx
            .send(ServerCommand::OfferIncoming {
                device_id,
                expected_generation: Some(generation),
                line_instance,
                call_id,
                info,
                presentation,
                ringer,
                delivery: Some(delivery),
            })
            .await
            .map_err(|_| ServerError::Stopped)?;
        Ok(IncomingOfferReceipt(receipt))
    }

    pub fn try_offer_incoming_call_for_session(
        &self,
        target: StationSessionTarget,
        line_instance: LineInstance,
        call_id: CallId,
        info: CallInfo,
        presentation: IncomingPresentation,
        ringer: Option<IncomingRing>,
    ) -> Result<IncomingOfferReceipt, ServerError> {
        let (delivery, receipt) = oneshot::channel();
        let StationSessionTarget {
            device_id,
            generation,
        } = target;
        self.command_tx
            .try_send(ServerCommand::OfferIncoming {
                device_id,
                expected_generation: Some(generation),
                line_instance,
                call_id,
                info,
                presentation,
                ringer,
                delivery: Some(delivery),
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => ServerError::CommandQueueFull,
                mpsc::error::TrySendError::Closed(_) => ServerError::Stopped,
            })?;
        Ok(IncomingOfferReceipt(receipt))
    }

    /// Request orderly server shutdown.
    ///
    /// Success means the request entered the queue. The owner must still await
    /// the [`Server::run`] future to know that it stopped accepting streams and
    /// issued disconnects to every registered session.
    pub async fn shutdown(&self) -> Result<(), ServerError> {
        self.command_tx
            .send(ServerCommand::Shutdown)
            .await
            .map_err(|_| ServerError::Stopped)
    }

    /// Atomically replace the configured station definitions. Only connected
    /// stations whose definition changed or was removed are asked to register
    /// again; unchanged live sessions and calls are preserved. Success means
    /// the replacement was committed and disconnect requests were queued, not
    /// that every affected transport has already closed.
    pub async fn reconfigure(
        &self,
        definitions: impl IntoIterator<Item = DeviceDefinition>,
    ) -> Result<ReconfigureResult, ServerError> {
        self.reconfigure_affected(definitions, []).await
    }

    /// Atomically replaces station definitions and reconnects the explicit
    /// set in addition to stations whose wire definition changed. This lets a
    /// higher-level configuration owner apply line or global policy changes
    /// whose effects are not represented in [`DeviceDefinition`].
    pub async fn reconfigure_affected(
        &self,
        definitions: impl IntoIterator<Item = DeviceDefinition>,
        affected: impl IntoIterator<Item = DeviceId>,
    ) -> Result<ReconfigureResult, ServerError> {
        let mut by_id = HashMap::new();
        for definition in definitions {
            definition.validate()?;
            by_id.insert(definition.id.clone(), definition);
        }
        let (applied_tx, applied_rx) = oneshot::channel();
        self.command_tx
            .send(ServerCommand::Reconfigure {
                definitions: by_id,
                affected: affected.into_iter().collect(),
                applied: applied_tx,
            })
            .await
            .map_err(|_| ServerError::Stopped)?;
        applied_rx.await.map_err(|_| ServerError::Stopped)
    }

    /// Commits station definitions and unknown-device admission as one server
    /// transaction before any affected session is disconnected.
    pub async fn reconfigure_station_policy(
        &self,
        definitions: impl IntoIterator<Item = DeviceDefinition>,
        affected: impl IntoIterator<Item = DeviceId>,
        anonymous_hotline: Option<AnonymousHotlineDefinition>,
    ) -> Result<ReconfigureResult, ServerError> {
        let mut by_id = HashMap::new();
        for definition in definitions {
            definition.validate()?;
            by_id.insert(definition.id.clone(), definition);
        }
        let (applied_tx, applied_rx) = oneshot::channel();
        self.command_tx
            .send(ServerCommand::ReconfigureStationPolicy {
                definitions: by_id,
                affected: affected.into_iter().collect(),
                anonymous_hotline,
                applied: applied_tx,
            })
            .await
            .map_err(|_| ServerError::Stopped)?;
        applied_rx.await.map_err(|_| ServerError::Stopped)
    }

    /// Replace the unknown-device guest template for future registrations.
    /// A changed policy disconnects only sessions that were admitted through
    /// the previous anonymous template; configured sessions are untouched. The
    /// returned count is the number of such sessions asked to disconnect.
    pub async fn reconfigure_anonymous_hotline(
        &self,
        definition: Option<AnonymousHotlineDefinition>,
    ) -> Result<usize, ServerError> {
        let (applied_tx, applied_rx) = oneshot::channel();
        self.command_tx
            .send(ServerCommand::ReconfigureAnonymousHotline {
                definition,
                applied: applied_tx,
            })
            .await
            .map_err(|_| ServerError::Stopped)?;
        applied_rx.await.map_err(|_| ServerError::Stopped)
    }
}

/// Stateful owner of station admission, registration, command dispatch, and
/// event correlation.
///
/// Construction is inert: callers must poll [`Self::run`]. The server owns its
/// listener or injected-ingress receiver and all registered session routing;
/// integration code normally retains only the returned [`ServerHandle`] and
/// event receiver after spawning the run future. Dropping the `Server` future
/// directly is abrupt, so normal shutdown should use [`ServerHandle::shutdown`]
/// and then await `run`.
#[derive(Debug)]
pub struct Server {
    listener: Option<TcpListener>,
    accepted_rx: mpsc::Receiver<AcceptedStation>,
    config: Arc<ServerConfig>,
    anonymous_hotline: Arc<RwLock<Option<AnonymousHotlineDefinition>>>,
    definitions: Arc<RwLock<HashMap<DeviceId, DeviceDefinition>>>,
    sessions: Sessions,
    lifecycle: Arc<Mutex<()>>,
    event_tx: mpsc::Sender<Event>,
    command_rx: mpsc::Receiver<ServerCommand>,
    next_generation: Arc<AtomicU64>,
    next_statistics_generation: Arc<AtomicU64>,
    next_call_id: Arc<AtomicU64>,
    latest_media_statistics: Arc<RwLock<HashMap<DeviceId, MediaStatisticsSnapshot>>>,
    call_answer_order: Arc<RwLock<CallSelectionOrder>>,
    observation_sink: ObservationSink,
    next_observation_connection_id: AtomicU64,
}

type Sessions = Arc<Mutex<HashMap<DeviceId, SessionSender>>>;
type CommandWriteConfirmation = oneshot::Sender<Result<(), String>>;
type IncomingOfferConfirmation = oneshot::Sender<IncomingOfferDelivery>;

#[derive(Clone, Debug)]
struct SessionSender {
    generation: SessionGeneration,
    anonymous_hotline: bool,
    tx: mpsc::Sender<SessionCommand>,
    admission: Arc<SessionAdmission>,
}

impl SessionSender {
    fn retire(&self) {
        self.admission.retire();
    }

    async fn send_if_active(&self, command: SessionCommand) -> Result<(), SessionCommand> {
        let mut retirement = self.admission.subscribe();
        if *retirement.borrow() == SessionAdmissionState::Retired {
            return Err(command);
        }
        tokio::select! {
            biased;
            _ = retirement.changed() => Err(command),
            permit = self.tx.reserve() => {
                let Ok(permit) = permit else {
                    return Err(command);
                };
                self.admission.commit(permit, command)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionAdmissionState {
    Active,
    Retired,
}

#[derive(Debug)]
struct SessionAdmission {
    state: SyncMutex<SessionAdmissionState>,
    retirement: watch::Sender<SessionAdmissionState>,
}

impl SessionAdmission {
    fn new() -> Self {
        let (retirement, _) = watch::channel(SessionAdmissionState::Active);
        Self {
            state: SyncMutex::new(SessionAdmissionState::Active),
            retirement,
        }
    }

    fn subscribe(&self) -> watch::Receiver<SessionAdmissionState> {
        self.retirement.subscribe()
    }

    fn retire(&self) {
        let mut state = self
            .state
            .lock()
            .expect("SCCP session admission lock poisoned");
        *state = SessionAdmissionState::Retired;
        self.retirement.send_replace(SessionAdmissionState::Retired);
    }

    fn commit(
        &self,
        permit: mpsc::Permit<'_, SessionCommand>,
        command: SessionCommand,
    ) -> Result<(), SessionCommand> {
        let state = self
            .state
            .lock()
            .expect("SCCP session admission lock poisoned");
        match *state {
            SessionAdmissionState::Active => {
                permit.send(command);
                Ok(())
            }
            SessionAdmissionState::Retired => Err(command),
        }
    }
}

#[derive(Debug)]
enum ServerCommand {
    Public(Box<Command>),
    Confirmed {
        command: Box<Command>,
        written: CommandWriteConfirmation,
        expires_at: Instant,
    },
    OfferIncoming {
        device_id: DeviceId,
        expected_generation: Option<SessionGeneration>,
        line_instance: LineInstance,
        call_id: CallId,
        info: CallInfo,
        presentation: IncomingPresentation,
        ringer: Option<IncomingRing>,
        delivery: Option<IncomingOfferConfirmation>,
    },
    Reconfigure {
        definitions: HashMap<DeviceId, DeviceDefinition>,
        affected: HashSet<DeviceId>,
        applied: oneshot::Sender<ReconfigureResult>,
    },
    ReconfigureStationPolicy {
        definitions: HashMap<DeviceId, DeviceDefinition>,
        affected: HashSet<DeviceId>,
        anonymous_hotline: Option<AnonymousHotlineDefinition>,
        applied: oneshot::Sender<ReconfigureResult>,
    },
    ReconfigureAnonymousHotline {
        definition: Option<AnonymousHotlineDefinition>,
        applied: oneshot::Sender<usize>,
    },
    Shutdown,
}

#[derive(Debug)]
enum AnonymousHotlineUpdate {
    Preserve,
    Replace(Option<AnonymousHotlineDefinition>),
}

#[derive(Debug)]
enum SessionCommand {
    Public(Box<Command>),
    Confirmed {
        command: Box<Command>,
        written: CommandWriteConfirmation,
        expires_at: Instant,
    },
    OfferIncoming {
        line_instance: LineInstance,
        call_id: CallId,
        info: Box<CallInfo>,
        presentation: IncomingPresentation,
        ringer: Option<IncomingRing>,
        delivery: Option<IncomingOfferConfirmation>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionDisposition {
    Continue,
    Terminate,
}

#[derive(Clone, Debug)]
struct SessionCall {
    call_id: CallId,
    wire_reference: u32,
    line_instance: u32,
    media: CallMedia,
    video_receive: VideoReceive,
    video_transmit: VideoTransmit,
    state: CallState,
    ringer: Option<IncomingRing>,
    history_disposition: CallHistoryDisposition,
    dialed_number: String,
    statistics_directory_number: String,
    transfer_role: Option<SessionTransferRole>,
}

#[derive(Clone, Debug, Default)]
struct VideoReceive {
    generation: u64,
    leg: Option<VideoReceiveLeg>,
}

#[derive(Clone, Debug)]
struct VideoReceiveLeg {
    request: MediaRequestIdentity,
    conference_id: ConferenceId,
    codec: Codec,
    requested_address_type: IpAddressType,
    state: MediaChannelState,
    deadline: Option<Instant>,
}

#[derive(Debug)]
struct ExpiredVideoReceive {
    call_id: CallId,
    codec: Codec,
    passthrough_party_id: PassthroughPartyId,
    close: ServerMessage,
}

#[derive(Clone, Debug, Default)]
struct VideoTransmit {
    generation: u64,
    leg: Option<VideoTransmitLeg>,
}

#[derive(Clone, Debug)]
struct VideoTransmitLeg {
    request: MediaRequestIdentity,
    conference_id: ConferenceId,
    codec: Codec,
    address_type: IpAddressType,
    state: MediaChannelState,
    deadline: Option<Instant>,
}

#[derive(Debug)]
struct ExpiredVideoTransmit {
    call_id: CallId,
    codec: Codec,
    passthrough_party_id: PassthroughPartyId,
    stop: ServerMessage,
}

#[derive(Clone, Debug)]
struct CallMedia {
    generation: u64,
    codec: Codec,
    packet_ms: u32,
    max_frames_per_packet: u32,
    receive: MediaLeg,
    transmit: MediaLeg,
    transmit_confirmation: TransmitConfirmation,
    /// Exact StartMediaTransmission endpoint paired with an outstanding
    /// OpenReceiveChannel in one outbound NAT compatibility transaction.
    /// A successful matching receive acknowledgement settles both halves.
    coupled_transmit_endpoint: Option<MediaEndpoint>,
    requested: bool,
}

impl CallMedia {
    fn new(codec: Codec) -> Self {
        Self {
            generation: 0,
            codec,
            packet_ms: DEFAULT_AUDIO_PACKET_MS,
            max_frames_per_packet: DEFAULT_AUDIO_MAX_FRAMES_PER_PACKET,
            receive: MediaLeg::default(),
            transmit: MediaLeg::default(),
            transmit_confirmation: TransmitConfirmation::Inactive,
            coupled_transmit_endpoint: None,
            requested: false,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct MediaLeg {
    request: Option<MediaRequestIdentity>,
    telephone_event_payload: u8,
    peer: Option<MediaEndpoint>,
    state: MediaChannelState,
    deadline: Option<Instant>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionTransferRole {
    Source { consultation_call_id: CallId },
    Consultation { source_call_id: CallId },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum MediaChannelState {
    #[default]
    Closed,
    Opening,
    Open,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum TransmitConfirmation {
    #[default]
    Inactive,
    Awaiting {
        deadline: Instant,
    },
    NotReported,
    Settled(TransmitOpenOutcome),
}

impl TransmitConfirmation {
    fn acknowledgement_is_reportable(self, status: MediaStatus) -> Option<bool> {
        match self {
            Self::Awaiting { .. } => Some(true),
            Self::NotReported => Some(status != MediaStatus::Ok),
            Self::Inactive | Self::Settled(_) => None,
        }
    }
}

impl MediaChannelState {
    const fn is_open(self) -> bool {
        matches!(self, Self::Open)
    }
}

fn validate_server_config(config: &ServerConfig) -> Result<(), ServerError> {
    if digit_character(config.dial_terminator).is_none() {
        return Err(ServerError::InvalidConfig(
            "dial terminator must be one DTMF character".into(),
        ));
    }
    if !(-840..=840).contains(&config.timezone_offset_minutes) {
        return Err(ServerError::InvalidConfig(
            "timezone offset must be between -840 and 840 minutes".into(),
        ));
    }
    if config.keepalive_seconds < 5 || config.secondary_keepalive_seconds < 5 {
        return Err(ServerError::InvalidConfig(
            "primary and secondary keepalive intervals must be at least 5 seconds".into(),
        ));
    }
    if config.advertised_address.is_unspecified()
        || config.advertised_address.is_multicast()
        || config
            .advertised_ipv6_address
            .is_some_and(|address| address.is_unspecified() || address.is_multicast())
    {
        return Err(ServerError::InvalidConfig(
            "advertised fallback addresses must be unicast".into(),
        ));
    }
    if !(MIN_REGISTRATION_BACKOFF..=MAX_REGISTRATION_BACKOFF)
        .contains(&config.registration_tokens.backoff)
    {
        return Err(ServerError::InvalidConfig(
            "registration-token backoff must be between 30 and 86400 seconds".into(),
        ));
    }
    if config.registration_tokens.server_priority == 0 {
        return Err(ServerError::InvalidConfig(
            "server priority must be nonzero".into(),
        ));
    }
    if config.signaling_servers.len() > crate::message::MAX_SIGNALING_SERVERS {
        return Err(ServerError::InvalidConfig(format!(
            "at most {} signaling servers may be advertised",
            crate::message::MAX_SIGNALING_SERVERS
        )));
    }
    let mut priorities = HashSet::new();
    for server in &config.signaling_servers {
        if server.priority == 0 || !priorities.insert(server.priority) {
            return Err(ServerError::InvalidConfig(
                "signaling server priorities must be nonzero and unique".into(),
            ));
        }
        if server.name.is_empty()
            || server.name.len() >= 48
            || server.name.chars().any(char::is_control)
            || server.address.is_unspecified()
            || server.address.is_multicast()
            || server.clear_port.is_none() && server.secure_port.is_none()
        {
            return Err(ServerError::InvalidConfig(
                "each signaling server requires a name, unicast address, and at least one port"
                    .into(),
            ));
        }
    }
    if !config.signaling_servers.is_empty()
        && !priorities.contains(&config.registration_tokens.server_priority)
    {
        return Err(ServerError::InvalidConfig(
            "the local server priority must occur in the advertised server list".into(),
        ));
    }
    config
        .signaling_qos
        .validate()
        .map_err(|error| ServerError::InvalidConfig(error.to_string()))
}

impl Server {
    /// Attaches a bounded, nonblocking stream of sanitized signaling records.
    ///
    /// Queue saturation never delays phone traffic. Whole observations are
    /// discarded and the next delivered item reports the loss count.
    pub fn with_observation_sender(mut self, sender: mpsc::Sender<ServerObservation>) -> Self {
        self.observation_sink = ObservationSink::new(sender);
        self
    }

    /// Bind the configured plain TCP endpoint and construct a server.
    ///
    /// The returned tuple contains the inert server, its cloneable command
    /// handle, and the sole event receiver. Call [`Self::local_addr`] after
    /// construction when `config.bind` used port zero, then spawn or await
    /// [`Self::run`]. This constructor classifies every accepted connection as
    /// [`StationTransport::Clear`]. Use [`Self::with_ingress`] when transport
    /// negotiation or multiple listeners are owned elsewhere.
    pub async fn bind(
        config: ServerConfig,
        definitions: impl IntoIterator<Item = DeviceDefinition>,
    ) -> Result<(Self, ServerHandle, mpsc::Receiver<Event>), ServerError> {
        validate_server_config(&config)?;
        let listener = TcpListener::bind(config.bind)
            .await
            .map_err(ServerError::Bind)?;
        if let Ok(local) = listener.local_addr() {
            match SignalingSocket::capture(&listener, local) {
                Ok(socket) => report_socket_qos(None, local, socket.apply(config.signaling_qos)),
                Err(error) => {
                    warn!(%local, %error, "unable to retain signaling listener QoS control")
                }
            }
        }
        let (server, handle, events, _) = Self::build(config, definitions, Some(listener))?;
        Ok((server, handle, events))
    }

    /// Construct a server whose ready station streams are supplied externally.
    ///
    /// The additional [`ServerIngress`] value is cloned by clear and secure
    /// listener tasks. Each task completes its transport setup, preserves the
    /// accepted peer and local socket addresses, and submits the stream with an
    /// accurate [`StationTransport`] classification. The returned server has no
    /// bound listener, so [`Self::local_addr`] is unavailable.
    pub fn with_ingress(
        config: ServerConfig,
        definitions: impl IntoIterator<Item = DeviceDefinition>,
    ) -> Result<(Self, ServerHandle, mpsc::Receiver<Event>, ServerIngress), ServerError> {
        Self::build(config, definitions, None)
    }

    fn build(
        config: ServerConfig,
        definitions: impl IntoIterator<Item = DeviceDefinition>,
        listener: Option<TcpListener>,
    ) -> Result<(Self, ServerHandle, mpsc::Receiver<Event>, ServerIngress), ServerError> {
        validate_server_config(&config)?;
        let mut by_id = HashMap::new();
        for definition in definitions {
            definition.validate()?;
            by_id.insert(definition.id.clone(), definition);
        }
        let (event_tx, event_rx) = mpsc::channel(EVENT_CAPACITY);
        let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (ingress, accepted_rx) =
            ServerIngress::channel(SESSION_ACCEPT_CAPACITY, config.signaling_qos);
        let next_call_id = Arc::new(AtomicU64::new(1));
        let latest_media_statistics = Arc::new(RwLock::new(HashMap::new()));
        let call_answer_order = Arc::new(RwLock::new(config.call_answer_order));
        let anonymous_hotline = Arc::new(RwLock::new(config.anonymous_hotline.clone()));
        let handle = ServerHandle {
            command_tx,
            next_call_id: Arc::clone(&next_call_id),
            latest_media_statistics: Arc::clone(&latest_media_statistics),
            call_answer_order: Arc::clone(&call_answer_order),
        };
        Ok((
            Self {
                listener,
                accepted_rx,
                config: Arc::new(config),
                anonymous_hotline,
                definitions: Arc::new(RwLock::new(by_id)),
                sessions: Arc::new(Mutex::new(HashMap::new())),
                lifecycle: Arc::new(Mutex::new(())),
                event_tx,
                command_rx,
                next_generation: Arc::new(AtomicU64::new(1)),
                next_statistics_generation: Arc::new(AtomicU64::new(1)),
                next_call_id,
                latest_media_statistics,
                call_answer_order,
                observation_sink: ObservationSink::default(),
                next_observation_connection_id: AtomicU64::new(1),
            },
            handle,
            event_rx,
            ingress,
        ))
    }

    /// Return the concrete address owned by [`Self::bind`].
    ///
    /// This exposes an operating-system-assigned port when the requested bind
    /// address used port zero. Servers created by [`Self::with_ingress`] return
    /// [`ServerError::InvalidConfig`] because listener addresses belong to the
    /// external transport owner.
    pub fn local_addr(&self) -> Result<SocketAddr, ServerError> {
        self.listener
            .as_ref()
            .ok_or_else(|| ServerError::InvalidConfig("server has no bound listener".into()))?
            .local_addr()
            .map_err(ServerError::Io)
    }

    /// Drive admission, command dispatch, reconfiguration, and shutdown.
    ///
    /// This consuming future must be polled exactly once. It accepts plain
    /// sockets owned by [`Self::bind`] and streams submitted through
    /// [`ServerIngress`], starts an independent session task for each, and
    /// serializes server-wide commands. It returns normally after an explicit
    /// shutdown request or after every [`ServerHandle`] is dropped; before
    /// returning it asks each registered session to disconnect. Listener or
    /// server-level I/O failures are returned as [`ServerError`], while an
    /// individual session failure is emitted as [`Event::SessionError`].
    pub async fn run(mut self) -> Result<(), ServerError> {
        if let Some(listener) = &self.listener {
            info!(bind = %listener.local_addr()?, "SCCP server listening");
        }
        loop {
            tokio::select! {
                accepted = accept_clear(self.listener.as_ref(), self.config.signaling_qos) => {
                    self.start_session(accepted?);
                }
                accepted = self.accepted_rx.recv(), if !self.accepted_rx.is_closed() => {
                    if let Some(accepted) = accepted {
                        self.start_session(accepted);
                    }
                }
                command = self.command_rx.recv() => {
                    match command {
                        Some(ServerCommand::Public(command)) => {
                            if let Err(error) = self.dispatch_public(*command).await {
                                warn!(%error, "discarding SCCP command for a retired session");
                            }
                        }
                        Some(ServerCommand::Confirmed { command, written, expires_at }) => {
                            self.dispatch_confirmed(command, written, expires_at).await;
                        }
                        Some(ServerCommand::OfferIncoming { device_id, expected_generation, line_instance, call_id, info, presentation, ringer, mut delivery }) => {
                            let session = self.sessions.lock().await.get(&device_id).cloned();
                            let Some(session) = session else {
                                if let Some(delivery) = delivery.take() {
                                    let _ = delivery.send(IncomingOfferDelivery::SessionMissing);
                                }
                                warn!(%device_id, "discarding incoming offer for a missing session");
                                continue;
                            };
                            if let Some(expected) = expected_generation
                                && session.generation != expected
                            {
                                if let Some(delivery) = delivery.take() {
                                    let _ = delivery.send(IncomingOfferDelivery::SessionStale {
                                        actual_generation: session.generation,
                                    });
                                }
                                warn!(%device_id, ?expected, actual = ?session.generation, "discarding incoming offer for a stale session generation");
                                continue;
                            }
                            if let Err(command) = session.send_if_active(SessionCommand::OfferIncoming {
                                line_instance,
                                call_id,
                                info: Box::new(info),
                                presentation,
                                ringer,
                                delivery,
                            }).await {
                                if let SessionCommand::OfferIncoming {
                                    delivery: Some(delivery),
                                    ..
                                } = command
                                {
                                    let outcome = self
                                        .unavailable_offer_delivery(&device_id, expected_generation)
                                        .await;
                                    let _ = delivery.send(outcome);
                                }
                                warn!(%device_id, "discarding incoming offer for a retired session");
                            }
                        }
                        Some(ServerCommand::Reconfigure { definitions, affected, applied }) => {
                            let result = self
                                .apply_station_policy(
                                    definitions,
                                    affected,
                                    AnonymousHotlineUpdate::Preserve,
                                )
                                .await;
                            let _ = applied.send(result);
                        }
                        Some(ServerCommand::ReconfigureStationPolicy {
                            definitions,
                            affected,
                            anonymous_hotline,
                            applied,
                        }) => {
                            let result = self
                                .apply_station_policy(
                                    definitions,
                                    affected,
                                    AnonymousHotlineUpdate::Replace(anonymous_hotline),
                                )
                                .await;
                            let _ = applied.send(result);
                        }
                        Some(ServerCommand::ReconfigureAnonymousHotline { definition, applied }) => {
                            let sessions = self.sessions.lock().await;
                            let changed = {
                                let mut current = self
                                    .anonymous_hotline
                                    .write()
                                    .expect("SCCP anonymous-hotline lock poisoned");
                                if *current == definition {
                                    false
                                } else {
                                    *current = definition;
                                    true
                                }
                            };
                            let affected = if changed {
                                sessions
                                    .values()
                                    .filter(|session| session.anonymous_hotline)
                                    .cloned()
                                    .collect::<Vec<_>>()
                            } else {
                                Vec::new()
                            };
                            drop(sessions);
                            let count = affected.len();
                            for session in affected {
                                session.retire();
                            }
                            let _ = applied.send(count);
                        }
                        Some(ServerCommand::Shutdown) | None => {
                            let sessions: Vec<_> = self.sessions.lock().await.values().cloned().collect();
                            for session in sessions { session.retire(); }
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    fn start_session(&self, accepted: AcceptedStation) {
        let AcceptedStation {
            stream,
            peer,
            local,
            transport,
            socket_qos,
        } = accepted;
        let observation_connection_id = self
            .next_observation_connection_id
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .ok()
            .and_then(ObservationConnectionId::new)
            .filter(|_| self.observation_sink.is_active());
        let stream: Box<dyn StationIo> = match observation_connection_id {
            Some(connection_id) => {
                self.observation_sink
                    .observe(ServerObservationKind::Connected {
                        connection_id,
                        peer,
                        local,
                        transport,
                    });
                Box::new(ObservedStationIo::new(
                    stream,
                    self.observation_sink.clone(),
                    connection_id,
                    peer,
                    local,
                    transport,
                ))
            }
            None => stream,
        };
        let context = SessionContext {
            peer,
            local,
            transport,
            socket_qos,
            config: Arc::clone(&self.config),
            definitions: Arc::clone(&self.definitions),
            anonymous_hotline: Arc::clone(&self.anonymous_hotline),
            sessions: Arc::clone(&self.sessions),
            lifecycle: Arc::clone(&self.lifecycle),
            event_tx: self.event_tx.clone(),
            next_generation: Arc::clone(&self.next_generation),
            next_statistics_generation: Arc::clone(&self.next_statistics_generation),
            next_call_id: Arc::clone(&self.next_call_id),
            latest_media_statistics: Arc::clone(&self.latest_media_statistics),
            call_answer_order: Arc::clone(&self.call_answer_order),
            observation_sink: self.observation_sink.clone(),
            observation_connection_id,
        };
        let error_tx = self.event_tx.clone();
        let observation_sink = self.observation_sink.clone();
        tokio::spawn(async move {
            let outcome = run_session(stream, context).await;
            if let Some(connection_id) = observation_connection_id {
                observation_sink.observe(ServerObservationKind::Disconnected {
                    connection_id,
                    reason: outcome.reason,
                });
            }
            match outcome.result {
                Ok(()) => debug!(%peer, "SCCP session ended cleanly"),
                Err(error) => {
                    warn!(%peer, %error, "SCCP session ended with an error");
                    let _ = error_tx
                        .send(Event::SessionError {
                            peer,
                            error: error.to_string(),
                        })
                        .await;
                }
            }
        });
    }

    async fn dispatch_public(&self, command: Command) -> Result<(), ServerError> {
        let device_id = command.device_id.clone();
        self.dispatch(&device_id, SessionCommand::Public(Box::new(command)))
            .await
    }

    async fn dispatch_confirmed(
        &self,
        command: Box<Command>,
        written: CommandWriteConfirmation,
        expires_at: Instant,
    ) {
        let device_id = command.device_id.clone();
        if confirmed_command_expired(&written, expires_at) {
            reject_expired_confirmed_command(written);
            return;
        }
        let session = self.sessions.lock().await.get(&device_id).cloned();
        let Some(session) = session else {
            let _ = written.send(Err(ServerError::DeviceNotConnected(device_id).to_string()));
            return;
        };
        if let Err(command) = session
            .send_if_active(SessionCommand::Confirmed {
                command,
                written,
                expires_at,
            })
            .await
        {
            let SessionCommand::Confirmed { written, .. } = command else {
                unreachable!("confirmed dispatch returned a different command variant")
            };
            let _ = written.send(Err(ServerError::DeviceNotConnected(device_id).to_string()));
        }
    }

    async fn dispatch(
        &self,
        device_id: &DeviceId,
        command: SessionCommand,
    ) -> Result<(), ServerError> {
        let session = self
            .sessions
            .lock()
            .await
            .get(device_id)
            .cloned()
            .ok_or_else(|| ServerError::DeviceNotConnected(device_id.clone()))?;
        session
            .send_if_active(command)
            .await
            .map_err(|_| ServerError::DeviceNotConnected(device_id.clone()))
    }

    async fn unavailable_offer_delivery(
        &self,
        device_id: &DeviceId,
        expected_generation: Option<SessionGeneration>,
    ) -> IncomingOfferDelivery {
        let current_generation = self
            .sessions
            .lock()
            .await
            .get(device_id)
            .map(|session| session.generation);
        match (expected_generation, current_generation) {
            (Some(expected), Some(actual)) if expected != actual => {
                IncomingOfferDelivery::SessionStale {
                    actual_generation: actual,
                }
            }
            _ => IncomingOfferDelivery::SessionMissing,
        }
    }

    async fn apply_station_policy(
        &self,
        definitions: HashMap<DeviceId, DeviceDefinition>,
        affected: HashSet<DeviceId>,
        anonymous_hotline: AnonymousHotlineUpdate,
    ) -> ReconfigureResult {
        // Registration takes the session and definition locks in the same
        // order, so it sees either the complete current policy or the complete
        // candidate policy.
        let sessions = self.sessions.lock().await;
        let result = {
            let mut current = self
                .definitions
                .write()
                .expect("SCCP definitions lock poisoned");
            let result = reconfigure_result(&current, &definitions, &affected);
            *current = definitions;
            result
        };
        let anonymous_changed = match anonymous_hotline {
            AnonymousHotlineUpdate::Preserve => false,
            AnonymousHotlineUpdate::Replace(next) => {
                let mut current = self
                    .anonymous_hotline
                    .write()
                    .expect("SCCP anonymous-hotline lock poisoned");
                if *current == next {
                    false
                } else {
                    *current = next;
                    true
                }
            }
        };
        let affected_devices = result
            .disconnected_devices()
            .chain(affected.iter())
            .cloned()
            .collect::<HashSet<_>>();
        let affected_sessions = sessions
            .iter()
            .filter(|(device, session)| {
                affected_devices.contains(*device)
                    || (anonymous_changed && session.anonymous_hotline)
            })
            .map(|(_, session)| session.clone())
            .collect::<Vec<_>>();
        drop(sessions);
        for session in affected_sessions {
            session.retire();
        }
        result
    }
}

fn confirmed_command_expired(written: &CommandWriteConfirmation, expires_at: Instant) -> bool {
    written.is_closed() || Instant::now() >= expires_at
}

fn reject_expired_confirmed_command(written: CommandWriteConfirmation) {
    let _ = written.send(Err(ServerError::CommandAcknowledgementTimeout.to_string()));
}

struct PreparedSessionCommand {
    command: SessionCommand,
    written: Option<CommandWriteConfirmation>,
    expires_at: Option<Instant>,
}

fn prepare_session_command(command: SessionCommand) -> Option<PreparedSessionCommand> {
    match command {
        SessionCommand::Confirmed {
            command,
            written,
            expires_at,
        } => {
            if confirmed_command_expired(&written, expires_at) {
                reject_expired_confirmed_command(written);
                None
            } else {
                Some(PreparedSessionCommand {
                    command: SessionCommand::Public(command),
                    written: Some(written),
                    expires_at: Some(expires_at),
                })
            }
        }
        command => Some(PreparedSessionCommand {
            command,
            written: None,
            expires_at: None,
        }),
    }
}

fn reconfigure_result(
    current: &HashMap<DeviceId, DeviceDefinition>,
    next: &HashMap<DeviceId, DeviceDefinition>,
    affected: &HashSet<DeviceId>,
) -> ReconfigureResult {
    let mut result = ReconfigureResult::default();
    for (device, definition) in next {
        match current.get(device) {
            None => result.added.push(device.clone()),
            Some(previous) if previous != definition => result.changed.push(device.clone()),
            Some(_) => {}
        }
    }
    let explicitly_changed: Vec<_> = affected
        .iter()
        .filter(|device| {
            current.contains_key(*device)
                && next.contains_key(*device)
                && !result.changed.contains(*device)
        })
        .cloned()
        .collect();
    result.changed.extend(explicitly_changed);
    result.removed.extend(
        current
            .keys()
            .filter(|device| !next.contains_key(*device))
            .cloned(),
    );
    result.added.sort();
    result.changed.sort();
    result.removed.sort();
    result
}

fn command_call_id(command: &Command) -> Option<CallId> {
    match &command.action {
        CommandAction::BeginCall { call_id, .. }
        | CommandAction::SetCallInfo { call_id, .. }
        | CommandAction::CommitOutboundCall { call_id, .. }
        | CommandAction::PresentOutboundProceeding { call_id, .. }
        | CommandAction::PresentOutboundRinging { call_id, .. }
        | CommandAction::SetCallState { call_id, .. }
        | CommandAction::SetCallSelected { call_id, .. }
        | CommandAction::DisplayPrompt { call_id, .. }
        | CommandAction::ClearPrompt { call_id, .. }
        | CommandAction::SetRecordingStatus { call_id, .. }
        | CommandAction::ShowConferenceParticipantActions { call_id, .. }
        | CommandAction::StartTone { call_id, .. }
        | CommandAction::StartRinging { call_id, .. }
        | CommandAction::StopRinging { call_id, .. }
        | CommandAction::OpenReceiveChannel { call_id, .. }
        | CommandAction::OpenMultimediaReceiveChannel { call_id, .. }
        | CommandAction::CloseMultimediaReceiveChannel { call_id, .. }
        | CommandAction::StartMultimediaTransmission { call_id, .. }
        | CommandAction::StopMultimediaTransmission { call_id, .. }
        | CommandAction::SetMultimediaTransmitBitRate { call_id, .. }
        | CommandAction::NotifyMultimediaTransmitBitRate { call_id, .. }
        | CommandAction::ControlMultimediaTransmission { call_id, .. }
        | CommandAction::OpenOutboundMedia { call_id, .. }
        | CommandAction::CloseReceiveChannel { call_id, .. }
        | CommandAction::StartMedia { call_id, .. }
        | CommandAction::StartMulticastReception { call_id, .. }
        | CommandAction::StopMulticastReception { call_id, .. }
        | CommandAction::StartMulticastTransmission { call_id, .. }
        | CommandAction::StopMulticastTransmission { call_id, .. }
        | CommandAction::StopMedia { call_id, .. }
        | CommandAction::CloseCall { call_id, .. } => Some(*call_id),
        CommandAction::BeginTransfer { source_call_id, .. } => Some(*source_call_id),
        CommandAction::SetMwi { .. }
        | CommandAction::SetStatusMessage { .. }
        | CommandAction::SetMicrophoneMode { .. }
        | CommandAction::ResetDevice { .. }
        | CommandAction::SetForwardStatus { .. }
        | CommandAction::SetFeatureStatus { .. }
        | CommandAction::SetDoNotDisturbStatus { .. }
        | CommandAction::SetRecordingButtonStatus { .. }
        | CommandAction::SetMobilityAppearance { .. }
        | CommandAction::SetBlfStatus { .. }
        | CommandAction::ShowParkingMenu { .. }
        | CommandAction::ShowConferenceList { .. }
        | CommandAction::ShowTextService { .. }
        | CommandAction::ShowInputService { .. }
        | CommandAction::ExecutePhoneActions { .. }
        | CommandAction::ShowImageService { .. }
        | CommandAction::ShowStatusService { .. }
        | CommandAction::SetBackgroundImage { .. }
        | CommandAction::PreviewBackgroundImage { .. }
        | CommandAction::SetRingtone { .. }
        | CommandAction::StartAnnouncement { .. }
        | CommandAction::StopAnnouncement { .. }
        | CommandAction::AnnouncementFinish { .. }
        | CommandAction::DisconnectDevice { .. } => None,
    }
}

async fn accept_clear(
    listener: Option<&TcpListener>,
    signaling_qos: SignalingQos,
) -> Result<AcceptedStation, ServerError> {
    let Some(listener) = listener else {
        return std::future::pending().await;
    };
    let (stream, peer) = listener.accept().await?;
    stream.set_nodelay(true)?;
    let local = stream.local_addr()?;
    let socket_qos = match SignalingSocket::capture(&stream, local) {
        Ok(socket) => {
            report_socket_qos(None, peer, socket.apply(signaling_qos));
            Some(Box::new(socket) as Box<dyn StationSocketQos>)
        }
        Err(error) => {
            warn!(%peer, %error, "unable to retain signaling socket QoS control");
            None
        }
    };
    Ok(AcceptedStation {
        stream: Box::new(stream),
        peer,
        local,
        transport: StationTransport::Clear,
        socket_qos,
    })
}

fn report_socket_qos(device_id: Option<&DeviceId>, endpoint: SocketAddr, report: SocketQosReport) {
    for failure in report.failures() {
        match device_id {
            Some(device_id) => {
                warn!(%device_id, %endpoint, %failure, "signaling socket marking unavailable")
            }
            None => warn!(%endpoint, %failure, "signaling socket marking unavailable"),
        }
    }
}

fn allocate_session_generation(
    next_generation: &AtomicU64,
) -> Result<SessionGeneration, ServerError> {
    let generation = next_generation
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            SessionGeneration::new(current).and_then(|_| current.checked_add(1))
        })
        .map_err(|_| ServerError::SessionGenerationExhausted)?;
    SessionGeneration::new(generation).ok_or(ServerError::SessionGenerationExhausted)
}

const fn transport_allowed(
    requirement: StationTransportRequirement,
    transport: StationTransport,
) -> bool {
    matches!(
        (requirement, transport),
        (StationTransportRequirement::Either, _)
            | (StationTransportRequirement::Clear, StationTransport::Clear)
            | (
                StationTransportRequirement::Secure,
                StationTransport::Secure
            )
    )
}

#[derive(Debug)]
struct SessionContext {
    peer: SocketAddr,
    local: SocketAddr,
    transport: StationTransport,
    socket_qos: Option<Box<dyn StationSocketQos>>,
    config: Arc<ServerConfig>,
    definitions: Arc<RwLock<HashMap<DeviceId, DeviceDefinition>>>,
    anonymous_hotline: Arc<RwLock<Option<AnonymousHotlineDefinition>>>,
    sessions: Sessions,
    lifecycle: Arc<Mutex<()>>,
    event_tx: mpsc::Sender<Event>,
    next_generation: Arc<AtomicU64>,
    next_statistics_generation: Arc<AtomicU64>,
    next_call_id: Arc<AtomicU64>,
    latest_media_statistics: Arc<RwLock<HashMap<DeviceId, MediaStatisticsSnapshot>>>,
    call_answer_order: Arc<RwLock<CallSelectionOrder>>,
    observation_sink: ObservationSink,
    observation_connection_id: Option<ObservationConnectionId>,
}

#[derive(Debug)]
struct SessionState {
    device: DeviceDefinition,
    registration: DeviceRegistration,
    features: PhoneFeatures,
    generation: SessionGeneration,
    runtime: SessionRuntimeState,
}

/// Context-free mutable bookkeeping for one already-valid station session.
///
/// Unlike `SessionState`, this value has a real neutral default: it contains
/// no device identity, peer address, negotiated protocol, or generation.
#[derive(Debug)]
struct SessionRuntimeState {
    calls_by_id: HashMap<CallId, SessionCall>,
    calls_by_wire: HashMap<u32, CallId>,
    media_capabilities: StationMediaCapabilities,
    next_media_token: Option<MediaRequestToken>,
    next_multicast_generation: u64,
    multicast: HashMap<MulticastKey, MulticastSession>,
    pending_connection_statistics: HashMap<u32, PendingConnectionStatistics>,
    statistics_references: HashSet<u32>,
    cancelled_calls: HashSet<CallId>,
    last_number_by_line: HashMap<u32, String>,
    forwarding_by_line: HashMap<u32, SessionForwarding>,
    feature_states: HashMap<u32, SessionFeatureState>,
    mwi_by_line: HashMap<u32, bool>,
    mobility_appearances: HashMap<u32, LineAppearance>,
    active_key_mode: KeyMode,
    active_call_id: Option<CallId>,
    ringer_owner: Option<CallId>,
    pending_parking_menu: Option<PendingParkingMenu>,
    active_blf_alerts: BTreeMap<u32, HandsetStatusMessage>,
    visible_blf_alert: Option<HandsetStatusMessage>,
    persistent_status_message: bool,
    headset_enabled: bool,
    media_path_states:
        HashMap<crate::message::values::MediaPathId, crate::message::values::MediaPathEvent>,
    pending_media_path_release: Option<PendingMediaPathRelease>,
    transport_writable: bool,
}

impl Default for SessionRuntimeState {
    fn default() -> Self {
        Self {
            calls_by_id: HashMap::new(),
            calls_by_wire: HashMap::new(),
            media_capabilities: StationMediaCapabilities::default(),
            next_media_token: MediaRequestToken::new(1),
            next_multicast_generation: 0,
            multicast: HashMap::new(),
            pending_connection_statistics: HashMap::new(),
            statistics_references: HashSet::new(),
            cancelled_calls: HashSet::new(),
            last_number_by_line: HashMap::new(),
            forwarding_by_line: HashMap::new(),
            feature_states: HashMap::new(),
            mwi_by_line: HashMap::new(),
            mobility_appearances: HashMap::new(),
            active_key_mode: KeyMode::OnHook,
            active_call_id: None,
            ringer_owner: None,
            pending_parking_menu: None,
            active_blf_alerts: BTreeMap::new(),
            visible_blf_alert: None,
            persistent_status_message: false,
            headset_enabled: false,
            media_path_states: HashMap::new(),
            pending_media_path_release: None,
            transport_writable: true,
        }
    }
}

impl SessionState {
    fn new(
        device: DeviceDefinition,
        registration: DeviceRegistration,
        features: PhoneFeatures,
        generation: SessionGeneration,
    ) -> Self {
        debug_assert_eq!(device.id, registration.id);
        Self {
            device,
            registration,
            features,
            generation,
            runtime: SessionRuntimeState::default(),
        }
    }
}

impl std::ops::Deref for SessionState {
    type Target = SessionRuntimeState;

    fn deref(&self) -> &Self::Target {
        &self.runtime
    }
}

impl std::ops::DerefMut for SessionState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.runtime
    }
}

#[derive(Clone, Copy, Debug)]
struct PendingMediaPathRelease {
    call_id: CallId,
    path: crate::message::values::MediaPathId,
    deadline: Instant,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct MulticastKey {
    conference_id: ConferenceId,
    call_id: CallId,
}

#[derive(Clone, Debug)]
struct MulticastSession {
    wire_call_reference: u32,
    receive: Option<MulticastReceive>,
    transmit: Option<MulticastTransmit>,
}

#[derive(Clone, Debug)]
struct MulticastReceive {
    request: MediaRequestIdentity,
    route: MulticastMediaRoute,
    state: MulticastReceiveState,
}

#[derive(Clone, Debug)]
enum MulticastReceiveState {
    AwaitingAcknowledgement { deadline: Instant },
    Open,
}

#[derive(Clone, Debug)]
struct MulticastTransmit {
    request: MediaRequestIdentity,
    route: MulticastMediaRoute,
}

impl SessionState {
    fn station_context(&self) -> StationSessionContext {
        StationSessionContext::new(self.registration.protocol, self.features)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SessionFeatureState {
    button_type: ButtonType,
    label: String,
    state: u32,
}

#[derive(Clone, Debug)]
struct PendingConnectionStatistics {
    session_generation: SessionGeneration,
    request_generation: u64,
    call_id: CallId,
    line_instance: u32,
    codec: Codec,
    packet_ms: u32,
    max_frames_per_packet: u32,
    receive_peer: Option<MediaEndpoint>,
    transmit_peer: Option<MediaEndpoint>,
    directory_number: String,
    processing: StatisticsProcessing,
    expires_at: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingParkingMenu {
    instance: u32,
    transaction_id: u32,
}

#[derive(Clone, Debug, Default)]
struct SessionForwarding {
    all: Option<String>,
    busy: Option<String>,
    no_answer: Option<String>,
}

async fn run_session(mut stream: Box<dyn StationIo>, context: SessionContext) -> SessionOutcome {
    let (session_tx, mut session_rx) = mpsc::channel(SESSION_COMMAND_CAPACITY);
    let admission = Arc::new(SessionAdmission::new());
    let mut retirement = admission.subscribe();
    let mut decoder = FrameDecoder::new();
    let mut read_buffer = [0_u8; 4096];
    let mut state: Option<SessionState> = None;
    let mut unhandled_command = None;
    let mut last_station_activity = Instant::now();
    let mut session_deadlines = tokio::time::interval(Duration::from_millis(100));
    session_deadlines.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let keepalive_seconds = if context.config.registration_tokens.server_priority == 1 {
        context.config.keepalive_seconds
    } else {
        context.config.secondary_keepalive_seconds
    };
    let keepalive_timeout = Duration::from_secs(u64::from(keepalive_seconds) * 3);

    let result = async {
        let reason = 'session: loop {
            if *retirement.borrow() == SessionAdmissionState::Retired {
                break StationDisconnectReason::ServerRetirement;
            }
            tokio::select! {
                read = stream.read(&mut read_buffer) => {
                    if *retirement.borrow() == SessionAdmissionState::Retired {
                        break StationDisconnectReason::ServerRetirement;
                    }
                    let count = read?;
                    if count == 0 {
                        break StationDisconnectReason::PeerClosure;
                    }
                    let frames = match decoder.push(&read_buffer[..count]) {
                        Ok(frames) => frames,
                        Err(error) if state.is_none() => {
                            debug!(
                                peer = %context.peer,
                                %error,
                                "discarding malformed pre-registration SCCP stream"
                            );
                            break StationDisconnectReason::ProtocolFailure;
                        }
                        Err(error) => return Err(error.into()),
                    };
                    for frame in frames {
                        if *retirement.borrow() == SessionAdmissionState::Retired {
                            break 'session StationDisconnectReason::ServerRetirement;
                        }
                        let decode_protocol = state
                            .as_ref()
                            .map_or(ProtocolVersion::V3, |state| state.registration.protocol);
                        let message_id = frame.message_id;
                        let message = match ClientMessage::decode_with_version(frame, decode_protocol) {
                            Ok(message) => message,
                            Err(error) if message_id != crate::message::wire_id::REGISTER => {
                                let device_id = state.as_ref().map(|state| state.device.id.clone());
                                warn!(peer = %context.peer, message_id = format_args!("0x{message_id:04x}"), %error, "ignoring malformed SCCP application message");
                                let _ = context.event_tx.send(Event::ProtocolWarning {
                                    peer: context.peer,
                                    device_id,
                                    message_id,
                                    error: error.to_string(),
                                }).await;
                                continue;
                            }
                            Err(error) => return Err(error.into()),
                        };
                        if let ClientMessage::Register(registration) = &message {
                            if state.is_some() {
                                return Err(ServerError::Protocol(CodecError::InvalidDefinition("duplicate REGISTER on one TCP session".into())));
                            }
                            match handle_registration(
                                &mut stream,
                                registration,
                                &context,
                                &session_tx,
                                &admission,
                            )
                            .await?
                            {
                                Some(registered) => {
                                    state = Some(registered.state);
                                    last_station_activity = Instant::now();
                                    let state = state
                                        .as_ref()
                                        .expect("registered session state was installed");
                                    info!(device_id = %state.device.id, protocol = %state.registration.protocol, peer = %context.peer, "SCCP device registered");
                                }
                                None => break 'session StationDisconnectReason::RegistrationRejected,
                            }
                        } else if let Some(state) = state.as_mut() {
                            last_station_activity = Instant::now();
                            if handle_registered_message(&mut stream, state, message, &context).await?
                                == SessionDisposition::Terminate
                            {
                                break 'session StationDisconnectReason::StationRequest;
                            }
                        } else if handle_pre_registration_message(&mut stream, message, &context).await?
                            == SessionDisposition::Terminate
                        {
                            break 'session StationDisconnectReason::RegistrationRejected;
                        }
                    }
                }
                command = session_rx.recv() => {
                    let Some(command) = command else {
                        break StationDisconnectReason::ServerRetirement;
                    };
                    if *retirement.borrow() == SessionAdmissionState::Retired {
                        unhandled_command = Some(command);
                        break StationDisconnectReason::ServerRetirement;
                    }
                    let Some(state) = state.as_mut() else { continue };
                    if handle_session_command_result(&mut stream, state, command, &context).await? {
                        break StationDisconnectReason::ServerRetirement;
                    }
                }
                changed = retirement.changed(), if state.is_some() => {
                    if changed.is_err() || *retirement.borrow() == SessionAdmissionState::Retired {
                        break StationDisconnectReason::ServerRetirement;
                    }
                }
                _ = session_deadlines.tick(), if state.is_some() => {
                    if *retirement.borrow() == SessionAdmissionState::Retired {
                        break StationDisconnectReason::ServerRetirement;
                    }
                    if let Some(state) = state.as_mut() {
                        handle_session_deadlines(&mut stream, state, &context, Instant::now()).await?;
                    }
                }
                _ = tokio::time::sleep_until(last_station_activity + keepalive_timeout), if state.is_some() => {
                    warn!(peer = %context.peer, "SCCP station activity timeout");
                    break StationDisconnectReason::KeepaliveExpiry;
                }
            }
        };
        Ok::<_, ServerError>(reason)
    }
    .await;

    let (reason, result) = match result {
        Ok(reason) => (reason, Ok(())),
        Err(error) => (disconnect_reason_for_error(&error), Err(error)),
    };

    admission.retire();
    if let Some(state) = state.as_ref() {
        reject_pending_session_commands(&mut session_rx, unhandled_command, state, &context).await;
    }
    if let Some(mut state) = state {
        finalize_session(&mut stream, &mut state, &context).await;
    }
    SessionOutcome { reason, result }
}

#[derive(Debug)]
struct SessionOutcome {
    reason: StationDisconnectReason,
    result: Result<(), ServerError>,
}

const fn disconnect_reason_for_error(error: &ServerError) -> StationDisconnectReason {
    match error {
        ServerError::Io(_) => StationDisconnectReason::IoFailure,
        ServerError::Protocol(_) | ServerError::PhoneXml(_) => {
            StationDisconnectReason::ProtocolFailure
        }
        _ => StationDisconnectReason::ServerFailure,
    }
}

async fn reject_pending_session_commands(
    session_rx: &mut mpsc::Receiver<SessionCommand>,
    first_command: Option<SessionCommand>,
    state: &SessionState,
    context: &SessionContext,
) {
    let current_generation = context
        .sessions
        .lock()
        .await
        .get(&state.device.id)
        .map(|session| session.generation);
    let offer_outcome = match current_generation {
        Some(actual_generation) if actual_generation != state.generation => {
            IncomingOfferDelivery::SessionStale { actual_generation }
        }
        _ => IncomingOfferDelivery::SessionMissing,
    };
    let reject = |command| match command {
        SessionCommand::Confirmed { written, .. } => {
            let _ = written.send(Err(ServerError::DeviceNotConnected(
                state.device.id.clone(),
            )
            .to_string()));
        }
        SessionCommand::OfferIncoming {
            delivery: Some(delivery),
            ..
        } => {
            let _ = delivery.send(offer_outcome.clone());
        }
        SessionCommand::Public(_) | SessionCommand::OfferIncoming { delivery: None, .. } => {}
    };
    if let Some(command) = first_command {
        reject(command);
    }
    while let Ok(command) = session_rx.try_recv() {
        reject(command);
    }
}

struct RegisteredSession {
    state: SessionState,
}

async fn finalize_session(
    stream: &mut dyn StationIo,
    state: &mut SessionState,
    context: &SessionContext,
) {
    if state.transport_writable {
        match tokio::time::timeout(
            SESSION_MEDIA_DRAIN_TIMEOUT,
            drain_session_media(stream, state),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                state.transport_writable = false;
                warn!(
                    device_id = %state.device.id,
                    session_generation = u64::from(state.generation),
                    %error,
                    "SCCP session media cleanup failed"
                );
            }
            Err(_) => {
                state.transport_writable = false;
                warn!(
                    device_id = %state.device.id,
                    session_generation = u64::from(state.generation),
                    "SCCP session media cleanup timed out"
                );
            }
        }
    }
    let event_permit = context.event_tx.reserve().await.ok();
    let _lifecycle = context.lifecycle.lock().await;
    let mut sessions = context.sessions.lock().await;
    let was_current = sessions
        .get(&state.device.id)
        .is_some_and(|entry| entry.generation == state.generation);
    if was_current {
        sessions.remove(&state.device.id);
    }
    drop(sessions);
    if was_current && let Some(event_permit) = event_permit {
        event_permit.send(Event::device(
            state.device.id.clone(),
            state.generation,
            DeviceEventKind::Disconnected {},
        ));
    }
}

async fn handle_registration(
    stream: &mut dyn StationIo,
    registration: &crate::message::RegistrationMessage,
    context: &SessionContext,
    session_tx: &mpsc::Sender<SessionCommand>,
    admission: &Arc<SessionAdmission>,
) -> Result<Option<RegisteredSession>, ServerError> {
    let configured = context
        .definitions
        .read()
        .expect("SCCP definitions lock poisoned")
        .get(&registration.device_id)
        .cloned();
    let anonymous_hotline = configured.is_none();
    let definition = configured.or_else(|| {
        context
            .anonymous_hotline
            .read()
            .expect("SCCP anonymous-hotline lock poisoned")
            .as_ref()
            .map(|hotline| hotline.device_definition(registration.device_id.clone()))
    });
    let Some(definition) = definition else {
        send_message(
            stream,
            &ServerMessage::RegisterReject {
                reason: "Device not configured".into(),
            },
            ProtocolVersion::V17,
        )
        .await?;
        return Ok(None);
    };
    if !transport_allowed(definition.transport, context.transport) {
        send_message(
            stream,
            &ServerMessage::RegisterReject {
                reason: "Device transport not permitted".into(),
            },
            ProtocolVersion::V17,
        )
        .await?;
        return Ok(None);
    }
    let protocol = registration
        .advertised_protocol
        .map(ProtocolVersion::negotiate)
        .transpose()?
        .unwrap_or(ProtocolVersion::V3);
    if canonical_ip_address(context.peer.ip()).is_ipv6() && protocol < ProtocolVersion::V17 {
        send_message(
            stream,
            &ServerMessage::RegisterReject {
                reason: "IPv6 requires protocol v17".into(),
            },
            protocol,
        )
        .await?;
        return Ok(None);
    }
    let features = registration.features;
    let generation = allocate_session_generation(&context.next_generation)?;
    if let Some(socket_qos) = &context.socket_qos {
        let signaling_qos = definition
            .signaling_qos
            .unwrap_or(context.config.signaling_qos);
        report_socket_qos(
            Some(&registration.device_id),
            context.peer,
            socket_qos.apply(signaling_qos),
        );
    }
    let device_registration = DeviceRegistration {
        id: registration.device_id.clone(),
        peer: context.peer,
        transport: context.transport,
        reported_address: registration.reported_address,
        reported_ipv6_address: registration.reported_ipv6_address,
        device_type: registration.device_type,
        protocol,
        firmware: registration.firmware.clone(),
    };
    send_message(
        stream,
        &ServerMessage::RegisterAck {
            keepalive_seconds: context.config.keepalive_seconds,
            secondary_keepalive_seconds: context.config.secondary_keepalive_seconds,
            protocol,
            features: PhoneFeatures::empty(),
            date_template: context.config.date_template.clone(),
        },
        protocol,
    )
    .await?;
    send_message(stream, &ServerMessage::CapabilitiesRequest, protocol).await?;
    let state = SessionState::new(definition, device_registration, features, generation);
    let registered = context
        .event_tx
        .reserve()
        .await
        .map_err(|_| ServerError::Stopped)?;
    let _lifecycle = context.lifecycle.lock().await;
    let mut sessions = context.sessions.lock().await;
    if let Some(previous) = sessions.get(&registration.device_id) {
        previous.retire();
    }
    sessions.insert(
        registration.device_id.clone(),
        SessionSender {
            generation,
            anonymous_hotline,
            tx: session_tx.clone(),
            admission: Arc::clone(admission),
        },
    );
    drop(sessions);
    registered.send(Event::device(
        state.device.id.clone(),
        state.generation,
        DeviceEventKind::Registered(state.registration.clone()),
    ));
    if let Some(connection_id) = context.observation_connection_id {
        context
            .observation_sink
            .observe(ServerObservationKind::Identified {
                connection_id,
                device_id: state.device.id.clone(),
                session_generation: state.generation,
            });
    }
    Ok(Some(RegisteredSession { state }))
}

async fn handle_session_command_result(
    stream: &mut dyn StationIo,
    state: &mut SessionState,
    command: SessionCommand,
    context: &SessionContext,
) -> Result<bool, ServerError> {
    let Some(PreparedSessionCommand {
        mut command,
        written,
        expires_at,
    }) = prepare_session_command(command)
    else {
        return Ok(false);
    };
    let offer_call_id = match &command {
        SessionCommand::OfferIncoming { call_id, .. } => Some(*call_id),
        _ => None,
    };
    let offer_delivery = match &mut command {
        SessionCommand::OfferIncoming { delivery, .. } => delivery.take(),
        _ => None,
    };
    if offer_call_id.is_some_and(|call_id| state.cancelled_calls.remove(&call_id)) {
        if let Some(delivery) = offer_delivery {
            let _ = delivery.send(IncomingOfferDelivery::CancelledBeforePresentation);
        }
        debug!(device_id = %state.device.id, ?offer_call_id, "discarding incoming call cancelled before it was offered");
        return Ok(false);
    }
    let result = match expires_at {
        Some(expires_at) => {
            match tokio::time::timeout_at(
                expires_at,
                handle_session_command(stream, state, command, context),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => {
                    state.transport_writable = false;
                    Err(ServerError::CommandAcknowledgementTimeout)
                }
            }
        }
        None => handle_session_command(stream, state, command, context).await,
    };
    match result {
        Ok(disconnect) => {
            if let Some(delivery) = offer_delivery {
                let _ = delivery.send(IncomingOfferDelivery::Presented);
            }
            if let Some(written) = written {
                let _ = written.send(Ok(()));
            }
            Ok(disconnect)
        }
        Err(error) => {
            if let Some(delivery) = offer_delivery {
                let _ = delivery.send(IncomingOfferDelivery::WriteFailed);
            }
            if let Some(written) = written {
                let _ = written.send(Err(error.to_string()));
            }
            if error.is_nonfatal_command_rejection() {
                warn!(
                    device_id = %state.device.id,
                    %error,
                    "rejected invalid SCCP station command"
                );
                Ok(false)
            } else {
                Err(error)
            }
        }
    }
}

async fn handle_session_deadlines(
    stream: &mut dyn StationIo,
    state: &mut SessionState,
    context: &SessionContext,
    now: Instant,
) -> Result<(), ServerError> {
    for expired in expire_handset_acknowledgements(&mut state.calls_by_id, now) {
        let (event, rollback_result) = match expired {
            ExpiredHandsetAcknowledgement::Receive { call_id } => {
                let rollback = prepare_audio_receive_rollback(state, call_id);
                let rollback_result = match rollback {
                    Some(rollback) => rollback_audio_receive(stream, state, rollback).await,
                    None => Ok(()),
                };
                warn!(
                    device_id = %state.device.id,
                    session_generation = u64::from(state.generation),
                    ?call_id,
                    "SCCP receive-channel acknowledgement deadline expired"
                );
                (
                    DeviceEventKind::HandsetAcknowledgementTimedOut {
                        call_id,
                        acknowledgement: HandsetAcknowledgement::OpenReceiveChannel,
                    },
                    rollback_result,
                )
            }
            ExpiredHandsetAcknowledgement::Transmit { call_id, endpoint } => (
                DeviceEventKind::TransmitChannelOpen {
                    call_id,
                    outcome: TransmitOpenOutcome::NotReported,
                    endpoint,
                },
                Ok(()),
            ),
        };
        context
            .event_tx
            .send(Event::device(
                state.device.id.clone(),
                state.generation,
                event,
            ))
            .await
            .map_err(|_| ServerError::Stopped)?;
        rollback_result?;
    }
    for (key, stop) in expire_multicast_reception_acknowledgements(state, now) {
        send_message(stream, &stop, state.registration.protocol).await?;
        context
            .event_tx
            .send(Event::device(
                state.device.id.clone(),
                state.generation,
                DeviceEventKind::MulticastReceptionTimedOut {
                    conference_id: key.conference_id,
                    call_id: key.call_id,
                },
            ))
            .await
            .map_err(|_| ServerError::Stopped)?;
    }
    for expired in expire_multimedia_receive_acknowledgements(state, now) {
        send_message(stream, &expired.close, state.registration.protocol).await?;
        context
            .event_tx
            .send(Event::device(
                state.device.id.clone(),
                state.generation,
                DeviceEventKind::MultimediaReceiveChannelTimedOut {
                    call_id: expired.call_id,
                    codec: expired.codec,
                    passthrough_party_id: expired.passthrough_party_id,
                },
            ))
            .await
            .map_err(|_| ServerError::Stopped)?;
    }
    for expired in expire_multimedia_transmit_acknowledgements(state, now) {
        send_message(stream, &expired.stop, state.registration.protocol).await?;
        context
            .event_tx
            .send(Event::device(
                state.device.id.clone(),
                state.generation,
                DeviceEventKind::MultimediaTransmitTimedOut {
                    call_id: expired.call_id,
                    codec: expired.codec,
                    passthrough_party_id: expired.passthrough_party_id,
                },
            ))
            .await
            .map_err(|_| ServerError::Stopped)?;
    }
    if let Some(pending) = state
        .pending_media_path_release
        .filter(|pending| pending.deadline <= now)
    {
        state.pending_media_path_release = None;
        let still_released = state.media_path_states.get(&pending.path)
            == Some(&crate::message::values::MediaPathEvent::Off)
            && !has_active_media_path(state)
            && active_media_path_call(state) == Some(pending.call_id);
        if still_released && let Some(call) = state.calls_by_id.get(&pending.call_id).cloned() {
            debug!(
                device_id = %state.device.id,
                call_id = ?call.call_id,
                path = ?pending.path,
                "completing unpaired media-path release as OnHook"
            );
            let line_instance = call.line_instance;
            complete_on_hook(stream, state, context, call, line_instance).await?;
        }
    }
    prune_connection_statistics(&mut state.pending_connection_statistics, now);
    Ok(())
}

fn expire_handset_acknowledgements(
    calls_by_id: &mut HashMap<CallId, SessionCall>,
    now: Instant,
) -> Vec<ExpiredHandsetAcknowledgement> {
    let mut calls = calls_by_id.keys().copied().collect::<Vec<_>>();
    calls.sort_unstable_by_key(|call_id| call_id.0);
    let mut expired = Vec::new();
    for call_id in calls {
        let call = calls_by_id
            .get_mut(&call_id)
            .expect("call identifier came from session state");
        if call.media.receive.state == MediaChannelState::Opening
            && call
                .media
                .receive
                .deadline
                .is_some_and(|deadline| deadline <= now)
        {
            call.media.receive.deadline = None;
            expired.push(ExpiredHandsetAcknowledgement::Receive { call_id });
            continue;
        }
        if matches!(
            call.media.transmit_confirmation,
            TransmitConfirmation::Awaiting { deadline } if deadline <= now
        ) {
            call.media.transmit_confirmation = TransmitConfirmation::NotReported;
            if let Some(endpoint) = call.media.transmit.peer {
                expired.push(ExpiredHandsetAcknowledgement::Transmit { call_id, endpoint });
            }
        }
    }
    expired
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExpiredHandsetAcknowledgement {
    Receive {
        call_id: CallId,
    },
    Transmit {
        call_id: CallId,
        endpoint: MediaEndpoint,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AudioReceiveRollback {
    call_id: CallId,
    wire_reference: u32,
    receive_request: Option<MediaRequestIdentity>,
    transmit_request: Option<MediaRequestIdentity>,
    coupled: bool,
}

fn prepare_audio_receive_rollback(
    state: &SessionState,
    call_id: CallId,
) -> Option<AudioReceiveRollback> {
    let call = state.calls_by_id.get(&call_id)?;
    (call.media.receive.state == MediaChannelState::Opening).then_some(AudioReceiveRollback {
        call_id,
        wire_reference: call.wire_reference,
        receive_request: call.media.receive.request,
        transmit_request: call.media.transmit.request,
        coupled: call.media.coupled_transmit_endpoint.is_some(),
    })
}

async fn rollback_audio_receive(
    stream: &mut dyn StationIo,
    state: &mut SessionState,
    rollback: AudioReceiveRollback,
) -> Result<(), ServerError> {
    match tokio::time::timeout(
        MEDIA_ROLLBACK_TIMEOUT,
        write_audio_receive_rollback(stream, state, rollback),
    )
    .await
    {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            state.transport_writable = false;
            Err(error)
        }
        Err(_) => {
            settle_audio_receive_rollback(state, rollback);
            state.transport_writable = false;
            warn!(
                device_id = %state.device.id,
                session_generation = u64::from(state.generation),
                call_id = ?rollback.call_id,
                "SCCP receive-channel rollback timed out"
            );
            Err(ServerError::MediaCleanupTimeout)
        }
    }
}

async fn write_audio_receive_rollback(
    stream: &mut dyn StationIo,
    state: &mut SessionState,
    rollback: AudioReceiveRollback,
) -> Result<(), ServerError> {
    let protocol = state.registration.protocol;
    let mut first_error = None;
    if rollback.coupled
        && let Err(error) = send_message(
            stream,
            &ServerMessage::StopMediaTransmission(AudioStreamControl {
                conference_id: ConferenceId::new(rollback.wire_reference),
                call_reference: CallReference::new(rollback.wire_reference),
                passthrough_party_id: media_request_party_id(
                    rollback.transmit_request,
                    rollback.wire_reference,
                )
                .into(),
                port_handling_flag: 0,
            }),
            protocol,
        )
        .await
    {
        first_error = Some(error);
    }
    let close_result = send_message(
        stream,
        &ServerMessage::CloseReceiveChannel(AudioStreamControl {
            conference_id: ConferenceId::new(rollback.wire_reference),
            call_reference: CallReference::new(rollback.wire_reference),
            passthrough_party_id: media_request_party_id(
                rollback.receive_request,
                rollback.wire_reference,
            )
            .into(),
            port_handling_flag: 0,
        }),
        protocol,
    )
    .await;
    if first_error.is_none() {
        first_error = close_result.err();
    }
    settle_audio_receive_rollback(state, rollback);
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn settle_audio_receive_rollback(state: &mut SessionState, rollback: AudioReceiveRollback) {
    if let Some(call) = state.calls_by_id.get_mut(&rollback.call_id)
        && call.media.receive.request == rollback.receive_request
    {
        call.media.receive.state = MediaChannelState::Closed;
        call.media.receive.deadline = None;
        call.media.receive.peer = None;
        if rollback.coupled && call.media.transmit.request == rollback.transmit_request {
            call.media.transmit.state = MediaChannelState::Closed;
            call.media.transmit.deadline = None;
            call.media.transmit.peer = None;
            call.media.transmit_confirmation = TransmitConfirmation::Inactive;
            call.media.coupled_transmit_endpoint = None;
        }
    }
}

async fn handle_pre_registration_message(
    stream: &mut dyn StationIo,
    message: ClientMessage,
    context: &SessionContext,
) -> Result<SessionDisposition, ServerError> {
    let mut disposition = SessionDisposition::Continue;
    match message {
        ClientMessage::KeepAlive => {
            send_message(stream, &ServerMessage::KeepAliveAck, ProtocolVersion::V3).await?;
        }
        ClientMessage::RegisterToken(token) => {
            let definition = context
                .definitions
                .read()
                .expect("SCCP definitions lock poisoned")
                .get(&token.device_id)
                .cloned();
            let configured = definition.is_some()
                || context
                    .anonymous_hotline
                    .read()
                    .expect("SCCP anonymous-hotline lock poisoned")
                    .is_some();
            let transport_permitted = definition.as_ref().is_none_or(|definition| {
                transport_allowed(definition.transport, context.transport)
            });
            let token_permitted = context.config.registration_tokens.accepts(&token.device_id);
            let incumbent = if configured && transport_permitted && token_permitted {
                context.sessions.lock().await.get(&token.device_id).cloned()
            } else {
                None
            };
            let (response, incumbent) = match incumbent {
                Some(incumbent) => (
                    ServerMessage::RegisterTokenReject {
                        backoff_seconds: REPLACEMENT_REGISTRATION_BACKOFF_SECONDS,
                    },
                    Some(incumbent),
                ),
                None if configured && transport_permitted && token_permitted => {
                    (ServerMessage::RegisterTokenAck, None)
                }
                None => (
                    ServerMessage::RegisterTokenReject {
                        backoff_seconds: u32::try_from(
                            context.config.registration_tokens.backoff.as_secs(),
                        )
                        .unwrap_or(u32::MAX),
                    },
                    None,
                ),
            };
            send_message(stream, &response, ProtocolVersion::V17).await?;
            if let Some(incumbent) = incumbent {
                let sessions = context.sessions.lock().await;
                if let Some(current) = sessions.get(&token.device_id)
                    && current.generation == incumbent.generation
                {
                    current.retire();
                }
                disposition = SessionDisposition::Terminate;
            }
        }
        ClientMessage::Alarm {
            severity,
            text,
            parameters,
        } => {
            debug!(peer = %context.peer, ?severity, %text, ?parameters, "pre-registration SCCP alarm");
        }
        ClientMessage::XmlAlarm(message) => match parse_phone_alarm(message.xml_bytes()) {
            Ok(telemetry) => {
                debug!(
                    peer = %context.peer,
                    payload_len = message.xml_bytes().len(),
                    summary = ?telemetry.summary(),
                    opaque = telemetry.is_opaque(),
                    "pre-registration SCCP XML alarm"
                );
            }
            Err(error) => {
                warn!(
                    peer = %context.peer,
                    payload_len = message.xml_bytes().len(),
                    %error,
                    "rejected pre-registration SCCP XML alarm"
                );
            }
        },
        ClientMessage::LocationInfo { xml } => match parse_phone_location(xml.as_bytes()) {
            Ok(telemetry) => {
                debug!(
                    peer = %context.peer,
                    payload_len = xml.len(),
                    summary = ?telemetry.summary(),
                    opaque = telemetry.is_opaque(),
                    "pre-registration SCCP location information"
                );
            }
            Err(error) => {
                warn!(
                    peer = %context.peer,
                    payload_len = xml.len(),
                    %error,
                    "rejected pre-registration SCCP location information"
                );
            }
        },
        message @ (ClientMessage::MediaPortList(_) | ClientMessage::SpcpRegisterToken(_)) => {
            debug!(peer = %context.peer, message = ?message, "pre-registration deferred SCCP message");
        }
        ClientMessage::KnownOpaque(message) => {
            debug!(peer = %context.peer, message = ?message, "pre-registration deferred SCCP message");
        }
        ClientMessage::Unknown(message) => {
            warn!(peer = %context.peer, message = ?message, "pre-registration unknown SCCP message");
        }
        ClientMessage::Register(_)
        | ClientMessage::IpPort { .. }
        | ClientMessage::KeypadButton { .. }
        | ClientMessage::EnblocCall { .. }
        | ClientMessage::Stimulus { .. }
        | ClientMessage::OffHook { .. }
        | ClientMessage::OnHook { .. }
        | ClientMessage::OffHookWithCallingParty { .. }
        | ClientMessage::LineStatRequest { .. }
        | ClientMessage::ConfigStatRequest
        | ClientMessage::TimeDateRequest
        | ClientMessage::ButtonTemplateRequest
        | ClientMessage::VersionRequest
        | ClientMessage::CapabilitiesResponse(_)
        | ClientMessage::CapabilitiesUpdate(_)
        | ClientMessage::OpenMultimediaReceiveChannelAck(_)
        | ClientMessage::ServerRequest
        | ClientMessage::MulticastMediaReceptionAck { .. }
        | ClientMessage::OpenReceiveChannelAck { .. }
        | ClientMessage::SoftKeySetRequest
        | ClientMessage::SoftKeyTemplateRequest
        | ClientMessage::SoftKeyEvent { .. }
        | ClientMessage::Unregister { .. }
        | ClientMessage::HookFlash { .. }
        | ClientMessage::ForwardStatusRequest { .. }
        | ClientMessage::SpeedDialStatusRequest { .. }
        | ClientMessage::ConnectionStatisticsResponse(_)
        | ClientMessage::HeadsetStatus { .. }
        | ClientMessage::MediaResourceNotification(_)
        | ClientMessage::MediaPathEvent { .. }
        | ClientMessage::MediaPathCapability { .. }
        | ClientMessage::MediaTransmissionFailure { .. }
        | ClientMessage::RegisterAvailableLines { .. }
        | ClientMessage::ServiceUrlStatusRequest { .. }
        | ClientMessage::FeatureStatusRequest { .. }
        | ClientMessage::StartMediaTransmissionAck(_)
        | ClientMessage::StartMultimediaTransmissionAck(_)
        | ClientMessage::ExtensionDeviceCapabilities(_)
        | ClientMessage::DeviceToUserData(_)
        | ClientMessage::DeviceToUserDataResponse(_)
        | ClientMessage::DeviceToUserDataV1(_)
        | ClientMessage::DeviceToUserDataResponseV1(_)
        | ClientMessage::PortResponse(_)
        | ClientMessage::SubscriptionStatusRequest(_)
        | ClientMessage::SubscribeDtmfPayloadResponse(_)
        | ClientMessage::UnsubscribeDtmfPayloadResponse(_)
        | ClientMessage::CallCountRequest(_)
        | ClientMessage::CreateConferenceResponse(_)
        | ClientMessage::DeleteConferenceResponse { .. }
        | ClientMessage::ModifyConferenceResponse(_)
        | ClientMessage::AuditConferenceResponse(_)
        | ClientMessage::AddParticipantResponse(_)
        | ClientMessage::AuditParticipantResponse(_) => {
            warn!(peer = %context.peer, message = ?message, "ignoring SCCP message before registration");
        }
    }
    Ok(disposition)
}

async fn handle_registered_message(
    stream: &mut dyn StationIo,
    state: &mut SessionState,
    message: ClientMessage,
    context: &SessionContext,
) -> Result<SessionDisposition, ServerError> {
    let disposition = if matches!(message, ClientMessage::Unregister { .. }) {
        SessionDisposition::Terminate
    } else {
        SessionDisposition::Continue
    };
    handle_client_message(stream, state, message, context).await?;
    Ok(disposition)
}

async fn handle_client_message(
    stream: &mut dyn StationIo,
    state: &mut SessionState,
    message: ClientMessage,
    context: &SessionContext,
) -> Result<(), ServerError> {
    let protocol = state.registration.protocol;
    match message {
        ClientMessage::KeepAlive => {
            send_message(stream, &ServerMessage::KeepAliveAck, protocol).await?
        }
        ClientMessage::CapabilitiesResponse(capabilities) => {
            let capabilities = StationMediaCapabilities::from(capabilities);
            state.media_capabilities.clone_from(&capabilities);
            context
                .event_tx
                .send(Event::device(
                    state.device.id.clone(),
                    state.generation,
                    DeviceEventKind::Capabilities { capabilities },
                ))
                .await
                .map_err(|_| ServerError::Stopped)?;
        }
        ClientMessage::CapabilitiesUpdate(update) => {
            let capabilities = update.into_media_capabilities();
            state.media_capabilities.clone_from(&capabilities);
            context
                .event_tx
                .send(Event::device(
                    state.device.id.clone(),
                    state.generation,
                    DeviceEventKind::Capabilities { capabilities },
                ))
                .await
                .map_err(|_| ServerError::Stopped)?;
        }
        ClientMessage::ConfigStatRequest => {
            send_station_ui_message(
                stream,
                state,
                &ServerMessage::ConfigStatus(crate::message::ConfigurationStatus {
                    device_name: state.device.id.as_str().to_owned(),
                    station_user_id: 0,
                    station_instance: 1,
                    user_name: state.device.description.clone(),
                    server_name: context.config.server_name.clone(),
                    line_count: state.device.line_count() as u32,
                    speed_dial_count: 0,
                }),
            )
            .await?;
        }
        ClientMessage::LineStatRequest { line_instance } => {
            if let Some(message) = line_status(&state.device, line_instance) {
                send_station_ui_message(stream, state, &message).await?;
            }
        }
        ClientMessage::ButtonTemplateRequest => {
            send_button_template(
                stream,
                &state.device,
                protocol,
                state.registration.device_type,
            )
            .await?;
        }
        ClientMessage::VersionRequest => {
            send_message(
                stream,
                &ServerMessage::Version {
                    firmware: context.config.firmware_version.clone(),
                },
                protocol,
            )
            .await?;
        }
        ClientMessage::ServerRequest => {
            send_message(
                stream,
                &ServerMessage::ServerResponse {
                    servers: server_response_endpoints(context, protocol)?,
                },
                protocol,
            )
            .await?;
        }
        ClientMessage::TimeDateRequest => {
            send_message(
                stream,
                &time_date_message(context.config.timezone_offset_minutes),
                protocol,
            )
            .await?
        }
        ClientMessage::SoftKeyTemplateRequest => {
            send_message(
                stream,
                &ServerMessage::SoftKeyTemplate {
                    actions: state.device.soft_keys.template_actions(),
                },
                protocol,
            )
            .await?
        }
        ClientMessage::SoftKeySetRequest => {
            send_message(
                stream,
                &ServerMessage::SoftKeySet {
                    profile: state.device.soft_keys.clone(),
                },
                protocol,
            )
            .await?
        }
        ClientMessage::ForwardStatusRequest { line_instance } => {
            let forwarding = state
                .forwarding_by_line
                .get(&line_instance)
                .cloned()
                .unwrap_or_default();
            send_message(
                stream,
                &ServerMessage::ForwardStatus {
                    line_instance,
                    forward_all: forwarding.all,
                    forward_busy: forwarding.busy,
                    forward_no_answer: forwarding.no_answer,
                },
                protocol,
            )
            .await?;
        }
        ClientMessage::SpeedDialStatusRequest {
            speed_dial_instance,
        } => {
            send_station_ui_message(
                stream,
                state,
                &speed_dial_status(&state.device, speed_dial_instance),
            )
            .await?;
        }
        ClientMessage::FeatureStatusRequest {
            index,
            capabilities,
        } => {
            if let Some(mut message) = feature_status_for_station(
                &state.device,
                index,
                capabilities,
                protocol,
                state.registration.device_type,
                state.features,
            ) {
                apply_cached_feature_projection(&state.feature_states, index, &mut message);
                send_station_ui_message(stream, state, &message).await?;
            }
        }
        ClientMessage::ServiceUrlStatusRequest { index } => {
            if let Some(message) = service_url_status(&state.device, index) {
                send_station_ui_message(stream, state, &message).await?;
            }
        }
        ClientMessage::SubscriptionStatusRequest(request) => {
            send_message(
                stream,
                &ServerMessage::SubscriptionStatus {
                    transaction_id: request.transaction_id,
                    feature_id: request.feature_id,
                    timer_seconds: 0,
                    cause: SubscriptionCause::RouteFailure,
                },
                protocol,
            )
            .await?;
        }
        ClientMessage::RegisterAvailableLines { .. } => {
            debug!(device_id = %state.device.id, "phone finished registering available lines");
        }
        ClientMessage::OffHook {
            line_instance,
            call_reference,
        } => {
            if let Some(active_call) = find_call(state, call_reference)
                && !matches!(
                    active_call.state,
                    CallState::RingIn | CallState::CallWaiting | CallState::OnHook
                )
            {
                debug!(
                    device_id = %state.device.id,
                    call_id = ?active_call.call_id,
                    call_state = ?active_call.state,
                    line_instance,
                    call_reference,
                    "ignoring duplicate OffHook while a call is already active"
                );
                return Ok(());
            }
            let line = normalize_line(state, line_instance);
            let answer = find_answer_call(
                state,
                call_reference,
                line_instance,
                *context
                    .call_answer_order
                    .read()
                    .expect("SCCP call-answer-order lock poisoned"),
            )
            .cloned();
            let answering = answer.is_some();
            let call = answer.unwrap_or_else(|| {
                ensure_phone_call(state, call_reference, line, &context.next_call_id)
            });
            if let Some(stored) = state.calls_by_id.get_mut(&call.call_id) {
                stored.state = CallState::OffHook;
            }
            state.active_call_id = Some(call.call_id);
            if answering {
                begin_answer_ui(stream, &call, protocol).await?;
            } else {
                state.active_key_mode = KeyMode::OffHook;
                begin_phone_call_ui(stream, &call, &state.device, state.station_context()).await?;
            }
            context
                .event_tx
                .send(Event::device(
                    state.device.id.clone(),
                    state.generation,
                    DeviceEventKind::OffHook {
                        call_id: call.call_id,
                        line_instance: LineInstance::new(line),
                    },
                ))
                .await
                .map_err(|_| ServerError::Stopped)?;
        }
        ClientMessage::OnHook {
            line_instance,
            call_reference,
        } => {
            state.pending_media_path_release = None;
            if let Some(call) = find_call(state, call_reference).cloned() {
                let line = if line_instance == 0 {
                    call.line_instance
                } else {
                    line_instance
                };
                complete_on_hook(stream, state, context, call, line).await?;
            }
        }
        ClientMessage::HookFlash {
            line_instance,
            call_reference,
        } => {
            let line_instance = normalize_line(state, line_instance);
            let call_id = find_call(state, call_reference).map(|call| call.call_id);
            context
                .event_tx
                .send(Event::device(
                    state.device.id.clone(),
                    state.generation,
                    DeviceEventKind::HookFlash {
                        call_id,
                        line_instance: LineInstance::new(line_instance),
                    },
                ))
                .await
                .map_err(|_| ServerError::Stopped)?;
        }
        ClientMessage::KeypadButton {
            button,
            call_reference,
            ..
        } => {
            if let Some(call) = find_call(state, call_reference) {
                if matches!(button, Digit::Unknown(_)) {
                    return Ok(());
                }
                let call = call.clone();
                if matches!(
                    call.state,
                    CallState::Connected
                        | CallState::Hold
                        | CallState::HoldYellow
                        | CallState::HoldRed
                ) && call.media.transmit.state.is_open()
                    && call.media.transmit.telephone_event_payload != 0
                {
                    // The handset sends connected-call digits in RTP when a
                    // telephone-event payload was negotiated. Forwarding the
                    // signaling copy would produce duplicate DTMF in the PBX.
                    return Ok(());
                }
                let collecting = matches!(call.state, CallState::OffHook | CallState::Transfer);
                if collecting && state.active_key_mode != KeyMode::DigitsFollowing {
                    state.active_key_mode = KeyMode::DigitsFollowing;
                    send_message(
                        stream,
                        &ServerMessage::StopTone {
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                        protocol,
                    )
                    .await?;
                    send_message(
                        stream,
                        &ServerMessage::SelectSoftKeys {
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                            set: KeyMode::DigitsFollowing,
                            valid_mask: state.device.soft_keys.valid_mask(KeyMode::DigitsFollowing),
                        },
                        protocol,
                    )
                    .await?;
                }
                if collecting && let Some(character) = digit_character(button) {
                    let number = if let Some(stored) = state.calls_by_id.get_mut(&call.call_id) {
                        stored.dialed_number.push(character);
                        stored.dialed_number.clone()
                    } else {
                        String::new()
                    };
                    if button == context.config.dial_terminator {
                        remember_last_number(state, call.line_instance, &number, &context.config);
                    }
                }
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::Digit {
                            call_id: call.call_id,
                            digit: button,
                        },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
            }
        }
        ClientMessage::EnblocCall {
            called_party,
            line_instance,
            ..
        } => {
            let line = normalize_line(state, line_instance);
            let existing = state
                .calls_by_id
                .values()
                .find(|call| call.line_instance == line && call.state != CallState::OnHook)
                .cloned();
            let created = existing.is_none();
            let call = existing
                .unwrap_or_else(|| ensure_phone_call(state, 0, line, &context.next_call_id));
            if created {
                state.active_key_mode = KeyMode::OffHook;
                begin_phone_call_ui(stream, &call, &state.device, state.station_context()).await?;
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::OffHook {
                            call_id: call.call_id,
                            line_instance: LineInstance::new(line),
                        },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
            }
            if let Some(stored) = state.calls_by_id.get_mut(&call.call_id) {
                stored.dialed_number.clone_from(&called_party);
            }
            remember_last_number(state, call.line_instance, &called_party, &context.config);
            context
                .event_tx
                .send(Event::device(
                    state.device.id.clone(),
                    state.generation,
                    DeviceEventKind::EnblocCall {
                        call_id: call.call_id,
                        line_instance: LineInstance::new(line),
                        number: called_party,
                    },
                ))
                .await
                .map_err(|_| ServerError::Stopped)?;
        }
        ClientMessage::SoftKeyEvent {
            event,
            line_instance,
            call_reference,
        } => {
            let received_soft_key = SoftKey::from(event);
            if !state
                .device
                .soft_keys
                .allows(state.active_key_mode, received_soft_key)
            {
                debug!(
                    device_id = %state.device.id,
                    mode = state.active_key_mode.wire_value(),
                    event,
                    "ignoring unavailable soft-key event"
                );
                return Ok(());
            }
            let line = normalize_line(state, line_instance);
            let mut soft_key = received_soft_key;
            let ringing_call = find_answer_call(
                state,
                call_reference,
                line_instance,
                *context
                    .call_answer_order
                    .read()
                    .expect("SCCP call-answer-order lock poisoned"),
            );
            let mut call_id = if matches!(soft_key, SoftKey::Answer | SoftKey::NewCall)
                && let Some(call) = ringing_call
            {
                soft_key = SoftKey::Answer;
                Some(call.call_id)
            } else {
                find_call(state, call_reference).map(|call| call.call_id)
            };
            if soft_key == SoftKey::MeetMe
                && call_id.is_some_and(|call_id| {
                    state
                        .calls_by_id
                        .get(&call_id)
                        .is_some_and(|call| call.state != CallState::OffHook)
                })
            {
                call_id = None;
            }
            if matches!(
                soft_key,
                SoftKey::NewCall | SoftKey::Pickup | SoftKey::GroupPickup | SoftKey::MeetMe
            ) && call_id.is_some_and(|call_id| {
                state
                    .calls_by_id
                    .get(&call_id)
                    .is_some_and(|call| call.state == CallState::OnHook)
            }) {
                call_id = None;
            }
            if soft_key == SoftKey::Redial {
                begin_redial(stream, state, context, line, call_id).await?;
                return Ok(());
            }
            if call_id.is_none()
                && matches!(
                    soft_key,
                    SoftKey::NewCall | SoftKey::Pickup | SoftKey::GroupPickup | SoftKey::MeetMe
                )
            {
                let call = if soft_key == SoftKey::MeetMe {
                    reserve_phone_call(state, line, &context.next_call_id)
                } else {
                    ensure_phone_call(state, 0, line, &context.next_call_id)
                };
                state.active_call_id = Some(call.call_id);
                state.active_key_mode = KeyMode::OffHook;
                begin_phone_call_ui(stream, &call, &state.device, state.station_context()).await?;
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::OffHook {
                            call_id: call.call_id,
                            line_instance: LineInstance::new(line),
                        },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
                call_id = Some(call.call_id);
            }
            if soft_key == SoftKey::Backspace
                && let Some(call_id) = call_id
                && let Some(call) = state.calls_by_id.get_mut(&call_id)
            {
                call.dialed_number.pop();
                let call = call.clone();
                send_message(
                    stream,
                    &ServerMessage::BackspaceResponse {
                        line_instance: call.line_instance,
                        call_reference: call.wire_reference,
                    },
                    protocol,
                )
                .await?;
            }
            if soft_key == SoftKey::Dial
                && let Some(call) = call_id.and_then(|call_id| state.calls_by_id.get(&call_id))
            {
                let line_instance = call.line_instance;
                let number = call.dialed_number.clone();
                remember_last_number(state, line_instance, &number, &context.config);
            }
            if soft_key == SoftKey::Answer
                && let Some(call) = call_id.and_then(|call_id| state.calls_by_id.get_mut(&call_id))
            {
                call.state = CallState::OffHook;
                let call = call.clone();
                state.active_call_id = Some(call.call_id);
                begin_answer_ui(stream, &call, protocol).await?;
            }
            context
                .event_tx
                .send(Event::device(
                    state.device.id.clone(),
                    state.generation,
                    DeviceEventKind::SoftKey {
                        call_id,
                        line_instance: LineInstance::new(line),
                        soft_key,
                    },
                ))
                .await
                .map_err(|_| ServerError::Stopped)?;
        }
        ClientMessage::Stimulus {
            stimulus,
            instance,
            call_reference,
            ..
        } => {
            let mut call_id = find_call(state, call_reference).map(|call| call.call_id);
            if stimulus == Stimulus::MeetMeConference
                && call_id.is_some_and(|call_id| {
                    state
                        .calls_by_id
                        .get(&call_id)
                        .is_some_and(|call| call.state != CallState::OffHook)
                })
            {
                call_id = None;
            }
            if matches!(
                stimulus,
                Stimulus::Line
                    | Stimulus::NewCall
                    | Stimulus::CallPickup
                    | Stimulus::GroupCallPickup
            ) && call_id.is_some_and(|call_id| {
                state
                    .calls_by_id
                    .get(&call_id)
                    .is_some_and(|call| call.state == CallState::OnHook)
            }) {
                call_id = None;
            }
            if stimulus == Stimulus::Line {
                let line = normalize_line(state, instance);
                if call_id.is_none() {
                    let call = ensure_phone_call(state, 0, line, &context.next_call_id);
                    state.active_key_mode = KeyMode::OffHook;
                    begin_phone_call_ui(stream, &call, &state.device, state.station_context())
                        .await?;
                    context
                        .event_tx
                        .send(Event::device(
                            state.device.id.clone(),
                            state.generation,
                            DeviceEventKind::OffHook {
                                call_id: call.call_id,
                                line_instance: LineInstance::new(line),
                            },
                        ))
                        .await
                        .map_err(|_| ServerError::Stopped)?;
                } else {
                    context
                        .event_tx
                        .send(Event::device(
                            state.device.id.clone(),
                            state.generation,
                            DeviceEventKind::LineButton {
                                line_instance: LineInstance::new(line),
                                call_id,
                            },
                        ))
                        .await
                        .map_err(|_| ServerError::Stopped)?;
                }
            } else if matches!(stimulus, Stimulus::SpeedDial | Stimulus::BlfSpeedDial) {
                let number = state.device.buttons.iter().find_map(|button| match button {
                    ButtonDefinition::SpeedDial(speed_dial) if speed_dial.instance == instance => {
                        Some(speed_dial.number.clone())
                    }
                    ButtonDefinition::BlfSpeedDial(speed_dial)
                        if speed_dial.instance == instance =>
                    {
                        Some(speed_dial.number.clone())
                    }
                    _ => None,
                });
                let Some(number) = number else {
                    debug!(
                        device_id = %state.device.id,
                        instance,
                        "ignoring unconfigured speed-dial button stimulus"
                    );
                    return Ok(());
                };

                // Reuse an existing outbound digit-collection call. Creating
                // a second call beside any other live call is permitted only
                // when the station advertised feature bit 30.
                let collecting_call = call_id
                    .and_then(|call_id| state.calls_by_id.get(&call_id))
                    .filter(|call| matches!(call.state, CallState::OffHook | CallState::Transfer))
                    .cloned();
                let has_live_call = state
                    .calls_by_id
                    .values()
                    .any(|call| call.state != CallState::OnHook);
                if collecting_call.is_none()
                    && has_live_call
                    && !state
                        .features
                        .contains(PhoneFeatures::MULTIPLE_ACTIVE_CALLS)
                {
                    debug!(
                        device_id = %state.device.id,
                        instance,
                        "ignoring speed-dial button beside an active call without multiple-active-call support"
                    );
                    return Ok(());
                }
                let (call, new_call) = collecting_call.map_or_else(
                    || {
                        let line = call_id
                            .and_then(|call_id| state.calls_by_id.get(&call_id))
                            .map_or_else(|| normalize_line(state, 0), |call| call.line_instance);
                        (reserve_phone_call(state, line, &context.next_call_id), true)
                    },
                    |call| (call, false),
                );

                if new_call {
                    state.active_call_id = Some(call.call_id);
                    state.active_key_mode = KeyMode::OffHook;
                    begin_phone_call_ui(stream, &call, &state.device, state.station_context())
                        .await?;
                    context
                        .event_tx
                        .send(Event::device(
                            state.device.id.clone(),
                            state.generation,
                            DeviceEventKind::OffHook {
                                call_id: call.call_id,
                                line_instance: LineInstance::new(call.line_instance),
                            },
                        ))
                        .await
                        .map_err(|_| ServerError::Stopped)?;
                }
                if let Some(stored) = state.calls_by_id.get_mut(&call.call_id) {
                    stored.dialed_number.clone_from(&number);
                }
                let await_further_digits = state.device.ui.speed_dial_await_further_digits;
                if await_further_digits {
                    state.active_key_mode = KeyMode::DigitsFollowing;
                    for message in [
                        ServerMessage::StopTone {
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                        ServerMessage::DialedNumber {
                            number: number.clone(),
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                        ServerMessage::SelectSoftKeys {
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                            set: KeyMode::DigitsFollowing,
                            valid_mask: state.device.soft_keys.valid_mask(KeyMode::DigitsFollowing),
                        },
                    ] {
                        send_station_ui_message(stream, state, &message).await?;
                    }
                }
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::SpeedDial {
                            call_id: call.call_id,
                            line_instance: LineInstance::new(call.line_instance),
                            number,
                            await_further_digits,
                        },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
            } else if stimulus == Stimulus::ParkingLot {
                let configured = state.device.buttons.iter().any(|button| {
                    matches!(
                        button,
                        ButtonDefinition::Feature(feature)
                            if feature.instance == instance
                                && feature.feature == ButtonType::ParkingLot
                    )
                });
                if !configured {
                    debug!(
                        device_id = %state.device.id,
                        instance,
                        "ignoring unconfigured parking-lot button stimulus"
                    );
                    return Ok(());
                }
                let line_instance = call_id
                    .and_then(|call_id| state.calls_by_id.get(&call_id))
                    .map_or_else(|| normalize_line(state, 0), |call| call.line_instance);
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::ParkingLotButton {
                            instance: LineInstance::new(instance),
                            call_id,
                            line_instance: LineInstance::new(line_instance),
                        },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
            } else if matches!(stimulus, Stimulus::Privacy | Stimulus::MultiblinkFeature)
                && state.device.recording_button(instance).is_some()
            {
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::RecordingButton {
                            instance: LineInstance::new(instance),
                        },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
            } else if stimulus == Stimulus::MultiblinkFeature {
                debug!(
                    device_id = %state.device.id,
                    instance,
                    "ignoring unconfigured recording-button stimulus"
                );
                return Ok(());
            } else if stimulus == Stimulus::Privacy {
                let configured = state.device.buttons.iter().any(|button| {
                    matches!(
                        button,
                        ButtonDefinition::Feature(feature)
                            if feature.instance == instance
                                && feature.feature == ButtonType::Feature
                    )
                });
                if !configured {
                    debug!(
                        device_id = %state.device.id,
                        instance,
                        "ignoring unconfigured generic feature-button stimulus"
                    );
                    return Ok(());
                }
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::FeatureButton {
                            instance: LineInstance::new(instance),
                        },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
            } else if stimulus == Stimulus::DoNotDisturb {
                let configured = state.device.buttons.iter().any(|button| {
                    matches!(
                        button,
                        ButtonDefinition::Feature(feature)
                            if feature.instance == instance
                                && feature.feature == ButtonType::DoNotDisturb
                    )
                });
                if !configured {
                    debug!(
                        device_id = %state.device.id,
                        instance,
                        "ignoring unconfigured do-not-disturb button stimulus"
                    );
                    return Ok(());
                }
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::DoNotDisturbButton {
                            instance: LineInstance::new(instance),
                        },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
            } else if stimulus == Stimulus::Mobility {
                let configured = state.device.buttons.iter().any(|button| {
                    matches!(
                        button,
                        ButtonDefinition::Feature(feature)
                            if feature.instance == instance
                                && feature.feature == ButtonType::Mobility
                    )
                });
                if !configured {
                    debug!(
                        device_id = %state.device.id,
                        instance,
                        "ignoring unconfigured mobility button stimulus"
                    );
                    return Ok(());
                }
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::MobilityButton {
                            instance: LineInstance::new(instance),
                        },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
            } else if matches!(stimulus, Stimulus::Voicemail | Stimulus::Messages) {
                // Cisco stations report the dedicated Messages key as either
                // the legacy Voicemail stimulus or the newer Messages
                // stimulus. Unlike a programmable feature key, that physical
                // key is not present in the server-provided button template,
                // so it must not require a matching ButtonDefinition.
                let line = call_id
                    .and_then(|call_id| state.calls_by_id.get(&call_id))
                    .map_or_else(
                        || normalize_line(state, instance),
                        |call| call.line_instance,
                    );
                let call = call_id
                    .and_then(|call_id| state.calls_by_id.get(&call_id).cloned())
                    .unwrap_or_else(|| ensure_phone_call(state, 0, line, &context.next_call_id));
                if call_id.is_none() {
                    state.active_call_id = Some(call.call_id);
                    state.active_key_mode = KeyMode::OffHook;
                    begin_phone_call_ui(stream, &call, &state.device, state.station_context())
                        .await?;
                    context
                        .event_tx
                        .send(Event::device(
                            state.device.id.clone(),
                            state.generation,
                            DeviceEventKind::OffHook {
                                call_id: call.call_id,
                                line_instance: LineInstance::new(line),
                            },
                        ))
                        .await
                        .map_err(|_| ServerError::Stopped)?;
                }
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::VoicemailButton {
                            call_id: call.call_id,
                            line_instance: LineInstance::new(line),
                        },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
            } else {
                let line = normalize_line(state, instance);
                let Some(soft_key) = stimulus_soft_key(stimulus) else {
                    debug!(
                        device_id = %state.device.id,
                        stimulus = stimulus.wire_value(),
                        "ignoring stimulus without a soft-key action mapping"
                    );
                    return Ok(());
                };
                if !state
                    .device
                    .soft_keys
                    .allows(state.active_key_mode, soft_key)
                {
                    debug!(
                        device_id = %state.device.id,
                        mode = state.active_key_mode.wire_value(),
                        stimulus = stimulus.wire_value(),
                        "ignoring unavailable soft-key stimulus"
                    );
                    return Ok(());
                }
                if soft_key == SoftKey::Redial {
                    begin_redial(stream, state, context, line, call_id).await?;
                    return Ok(());
                }
                if matches!(
                    soft_key,
                    SoftKey::NewCall | SoftKey::Pickup | SoftKey::GroupPickup | SoftKey::MeetMe
                ) && call_id.is_none()
                {
                    let call = if soft_key == SoftKey::MeetMe {
                        reserve_phone_call(state, line, &context.next_call_id)
                    } else {
                        ensure_phone_call(state, 0, line, &context.next_call_id)
                    };
                    state.active_call_id = Some(call.call_id);
                    state.active_key_mode = KeyMode::OffHook;
                    begin_phone_call_ui(stream, &call, &state.device, state.station_context())
                        .await?;
                    context
                        .event_tx
                        .send(Event::device(
                            state.device.id.clone(),
                            state.generation,
                            DeviceEventKind::OffHook {
                                call_id: call.call_id,
                                line_instance: LineInstance::new(line),
                            },
                        ))
                        .await
                        .map_err(|_| ServerError::Stopped)?;
                    call_id = Some(call.call_id);
                }
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::SoftKey {
                            call_id,
                            line_instance: LineInstance::new(line),
                            soft_key,
                        },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
            }
        }
        ClientMessage::MulticastMediaReceptionAck {
            status,
            passthrough_party_id,
            call_reference,
        } => {
            let Some(key) =
                find_multicast_receive_key(state, call_reference.get(), passthrough_party_id.get())
            else {
                debug!(
                    device_id = %state.device.id,
                    "ignored stale or mismatched multicast reception acknowledgement"
                );
                return Ok(());
            };
            if status == MediaStatus::Ok {
                let route = {
                    let receive = state
                        .multicast
                        .get_mut(&key)
                        .and_then(|session| session.receive.as_mut())
                        .expect("multicast key came from current receive state");
                    receive.state = MulticastReceiveState::Open;
                    receive.route
                };
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::MulticastReceptionStarted {
                            conference_id: key.conference_id,
                            call_id: key.call_id,
                            route,
                        },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
            } else {
                if let Some(stop) = take_multicast_stop(state, key, true) {
                    send_message(stream, &stop, protocol).await?;
                }
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::MulticastReceptionFailed {
                            conference_id: key.conference_id,
                            call_id: key.call_id,
                            status,
                        },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
            }
        }
        ClientMessage::OpenReceiveChannelAck {
            status,
            address,
            port,
            call_reference,
            passthrough_party_id,
        } => {
            if let Some(call_id) =
                find_receive_media_call_id(state, call_reference, passthrough_party_id)
            {
                let call = state
                    .calls_by_id
                    .get(&call_id)
                    .expect("media call identifier came from session state")
                    .clone();
                if call.media.receive.state != MediaChannelState::Opening {
                    debug!(
                        device_id = %state.device.id,
                        call_id = ?call.call_id,
                        state = ?call.media.receive.state,
                        "ignored stale receive-channel acknowledgement"
                    );
                    return Ok(());
                }
                let endpoint = MediaEndpoint {
                    address,
                    rtp_port: port,
                    rtcp_port: port.saturating_add(1),
                    codec: call.media.codec,
                    packet_ms: call.media.packet_ms,
                    max_frames_per_packet: call.media.max_frames_per_packet,
                    telephone_event_payload: call.media.receive.telephone_event_payload,
                };
                let (implied_transmit, rollback_result) = if status == MediaStatus::Ok {
                    let stored = state
                        .calls_by_id
                        .get_mut(&call_id)
                        .expect("media call identifier came from session state");
                    stored.media.receive.state = MediaChannelState::Open;
                    stored.media.receive.peer = Some(endpoint);
                    stored.media.receive.deadline = None;
                    if let Some(endpoint) = stored.media.coupled_transmit_endpoint.take() {
                        stored.media.transmit.state = MediaChannelState::Open;
                        stored.media.transmit.peer = Some(endpoint);
                        stored.media.transmit.deadline = None;
                        stored.media.transmit_confirmation =
                            TransmitConfirmation::Settled(TransmitOpenOutcome::Implied);
                        (Some(endpoint), Ok(()))
                    } else {
                        (None, Ok(()))
                    }
                } else {
                    let rollback_result = match prepare_audio_receive_rollback(state, call_id) {
                        Some(rollback) => rollback_audio_receive(stream, state, rollback).await,
                        None => Ok(()),
                    };
                    (None, rollback_result)
                };
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::ReceiveChannelOpened {
                            call_id: call.call_id,
                            status,
                            endpoint,
                        },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
                rollback_result?;
                if let Some(endpoint) = implied_transmit {
                    context
                        .event_tx
                        .send(Event::device(
                            state.device.id.clone(),
                            state.generation,
                            DeviceEventKind::TransmitChannelOpen {
                                call_id: call.call_id,
                                outcome: TransmitOpenOutcome::Implied,
                                endpoint,
                            },
                        ))
                        .await
                        .map_err(|_| ServerError::Stopped)?;
                }
            }
        }
        ClientMessage::StartMediaTransmissionAck(ack) => {
            if let Some(call_id) = find_transmit_media_call_id(
                state,
                ack.conference_id,
                ack.call_reference,
                ack.passthrough_party_id,
            ) {
                let call = state
                    .calls_by_id
                    .get(&call_id)
                    .expect("media call identifier came from session state")
                    .clone();
                let Some(report_outcome) = call
                    .media
                    .transmit_confirmation
                    .acknowledgement_is_reportable(ack.status)
                else {
                    debug!(
                        device_id = %state.device.id,
                        call_id = ?call.call_id,
                        confirmation = ?call.media.transmit_confirmation,
                        "ignored stale transmit-channel acknowledgement"
                    );
                    return Ok(());
                };
                if call.media.transmit.state == MediaChannelState::Closed {
                    debug!(
                        device_id = %state.device.id,
                        call_id = ?call.call_id,
                        confirmation = ?call.media.transmit_confirmation,
                        "ignored stale transmit-channel acknowledgement"
                    );
                    return Ok(());
                }
                let endpoint = MediaEndpoint {
                    address: ack.address,
                    rtp_port: ack.port,
                    rtcp_port: ack.port.saturating_add(1),
                    codec: call.media.codec,
                    packet_ms: call.media.packet_ms,
                    max_frames_per_packet: call.media.max_frames_per_packet,
                    telephone_event_payload: call.media.transmit.telephone_event_payload,
                };
                let coupled = call.media.coupled_transmit_endpoint.is_some();
                let rollback = if ack.status != MediaStatus::Ok && coupled {
                    prepare_audio_receive_rollback(state, call_id)
                } else {
                    None
                };
                let rollback_result = match rollback {
                    Some(rollback) => rollback_audio_receive(stream, state, rollback).await,
                    None => Ok(()),
                };
                let stored = state
                    .calls_by_id
                    .get_mut(&call_id)
                    .expect("media call identifier came from session state");
                let outcome = match ack.status {
                    MediaStatus::Ok => {
                        stored.media.coupled_transmit_endpoint = None;
                        stored.media.transmit.state = MediaChannelState::Open;
                        stored.media.transmit.peer = Some(endpoint);
                        TransmitOpenOutcome::Acknowledged
                    }
                    status => {
                        stored.media.transmit.state = MediaChannelState::Closed;
                        stored.media.transmit.peer = None;
                        if coupled && rollback.is_none() {
                            stored.media.receive.state = MediaChannelState::Closed;
                            stored.media.receive.deadline = None;
                            stored.media.receive.peer = None;
                            stored.media.coupled_transmit_endpoint = None;
                        }
                        TransmitOpenOutcome::Rejected(status)
                    }
                };
                stored.media.transmit.deadline = None;
                stored.media.transmit_confirmation = TransmitConfirmation::Settled(outcome);
                if report_outcome {
                    context
                        .event_tx
                        .send(Event::device(
                            state.device.id.clone(),
                            state.generation,
                            DeviceEventKind::TransmitChannelOpen {
                                call_id: call.call_id,
                                outcome,
                                endpoint,
                            },
                        ))
                        .await
                        .map_err(|_| ServerError::Stopped)?;
                }
                rollback_result?;
            }
        }
        ClientMessage::Alarm {
            severity,
            text,
            parameters,
        } => {
            context
                .event_tx
                .send(Event::device(
                    state.device.id.clone(),
                    state.generation,
                    DeviceEventKind::Alarm {
                        severity,
                        text,
                        parameters,
                    },
                ))
                .await
                .map_err(|_| ServerError::Stopped)?;
        }
        ClientMessage::XmlAlarm(message) => match parse_phone_alarm(message.xml_bytes()) {
            Ok(telemetry) => {
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::XmlAlarm { telemetry },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
            }
            Err(error) => {
                warn!(
                    device_id = %state.device.id,
                    payload_len = message.xml_bytes().len(),
                    %error,
                    "rejected SCCP XML alarm"
                );
            }
        },
        ClientMessage::LocationInfo { xml } => match parse_phone_location(xml.as_bytes()) {
            Ok(telemetry) => {
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::LocationInformation { telemetry },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
            }
            Err(error) => {
                warn!(
                    device_id = %state.device.id,
                    payload_len = xml.len(),
                    %error,
                    "rejected SCCP location information"
                );
            }
        },
        ClientMessage::Unregister { .. } => {
            send_message(stream, &ServerMessage::UnregisterAck, protocol).await?;
        }
        ClientMessage::CallCountRequest(_) => {
            let response = call_count_response(&state.device)?;
            send_message(stream, &response, protocol).await?;
        }
        ClientMessage::ConnectionStatisticsResponse(statistics) => {
            collect_connection_statistics(state, statistics, context).await?;
        }
        ClientMessage::MediaTransmissionFailure {
            conference_id,
            passthrough_party_id,
            address,
            port,
            call_reference,
            status,
        } => {
            if let Some(key) = find_multicast_transmit_key(
                state,
                conference_id,
                call_reference,
                passthrough_party_id,
                address,
                port,
            ) {
                if let Some(stop) = take_multicast_stop(state, key, false) {
                    send_message(stream, &stop, protocol).await?;
                }
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::MulticastTransmissionFailed {
                            conference_id: key.conference_id,
                            call_id: key.call_id,
                            status,
                            address,
                            port,
                        },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
                return Ok(());
            }
            let Some(call_id) = find_transmit_media_call_id(
                state,
                conference_id,
                call_reference,
                passthrough_party_id,
            ) else {
                return Ok(());
            };
            let call = state
                .calls_by_id
                .get(&call_id)
                .expect("media call identifier came from session state")
                .clone();
            let Some(endpoint) = call.media.transmit.peer else {
                return Ok(());
            };
            if call.media.transmit.state != MediaChannelState::Open
                || (conference_id != 0 && conference_id != call.wire_reference)
                || endpoint.address != address
                || endpoint.rtp_port != port
            {
                debug!(
                    device_id = %state.device.id,
                    call_id = ?call.call_id,
                    "ignored stale or mismatched media-transmission failure"
                );
                return Ok(());
            }
            let stored = state
                .calls_by_id
                .get_mut(&call_id)
                .expect("media call identifier came from session state");
            stored.media.transmit.state = MediaChannelState::Closed;
            stored.media.transmit.peer = None;
            stored.media.transmit_confirmation = TransmitConfirmation::Inactive;
            context
                .event_tx
                .send(Event::device(
                    state.device.id.clone(),
                    state.generation,
                    DeviceEventKind::MediaTransmissionFailed {
                        call_id,
                        status,
                        endpoint,
                    },
                ))
                .await
                .map_err(|_| ServerError::Stopped)?;
        }
        ClientMessage::HeadsetStatus { enabled } => {
            if state.headset_enabled != enabled {
                state.headset_enabled = enabled;
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::HeadsetStatusChanged { enabled },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
            }
        }
        ClientMessage::MediaPathEvent {
            path,
            event: media_path_event,
        } => {
            if state.media_path_states.get(&path) != Some(&media_path_event) {
                state.media_path_states.insert(path, media_path_event);
                if media_path_event == crate::message::values::MediaPathEvent::On {
                    state.pending_media_path_release = None;
                } else if media_path_event == crate::message::values::MediaPathEvent::Off
                    && is_local_audio_path(path)
                    && !has_active_media_path(state)
                    && let Some(call_id) = active_media_path_call(state)
                {
                    state.pending_media_path_release = Some(PendingMediaPathRelease {
                        call_id,
                        path,
                        deadline: Instant::now() + MEDIA_PATH_RELEASE_GRACE,
                    });
                }
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::MediaPathChanged {
                            path,
                            event: media_path_event,
                        },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
            }
        }
        ClientMessage::MediaPathCapability { .. } => {}
        message @ (ClientMessage::IpPort { .. }
        | ClientMessage::OffHookWithCallingParty { .. }
        | ClientMessage::MediaResourceNotification(_)
        | ClientMessage::SubscribeDtmfPayloadResponse(_)
        | ClientMessage::UnsubscribeDtmfPayloadResponse(_)
        | ClientMessage::PortResponse(_)) => {
            debug!(device_id = %state.device.id, message = ?message, "consumed SCCP telemetry");
        }
        ClientMessage::DeviceToUserData(message) => {
            handle_phone_service_message(
                state,
                context,
                crate::message::wire_id::DEVICE_TO_USER_DATA,
                PhoneServiceMessageKind::Data,
                PhoneServiceRouting {
                    application_id: ApplicationId::new(message.application_id),
                    line_instance: LineInstance::new(message.line_instance),
                    call_reference: CallReference::new(message.call_reference),
                    transaction_id: TransactionId::new(message.transaction_id),
                },
                None,
                &message.data,
            )
            .await?;
        }
        ClientMessage::DeviceToUserDataResponse(message) => {
            handle_phone_service_message(
                state,
                context,
                crate::message::wire_id::DEVICE_TO_USER_DATA_RESPONSE,
                PhoneServiceMessageKind::Response,
                PhoneServiceRouting {
                    application_id: ApplicationId::new(message.application_id),
                    line_instance: LineInstance::new(message.line_instance),
                    call_reference: CallReference::new(message.call_reference),
                    transaction_id: TransactionId::new(message.transaction_id),
                },
                None,
                &message.data,
            )
            .await?;
        }
        ClientMessage::DeviceToUserDataV1(message) => {
            handle_phone_service_message(
                state,
                context,
                crate::message::wire_id::DEVICE_TO_USER_DATA_V1,
                PhoneServiceMessageKind::Data,
                PhoneServiceRouting {
                    application_id: ApplicationId::new(message.application_id),
                    line_instance: LineInstance::new(message.line_instance),
                    call_reference: CallReference::new(message.call_reference),
                    transaction_id: TransactionId::new(message.transaction_id),
                },
                Some(PhoneServiceExtendedRouting {
                    sequence_flag: message.sequence_flag,
                    display_priority: message.display_priority,
                    conference_id: message.conference_id,
                    application_instance_id: message.application_instance_id,
                    routing: message.routing,
                }),
                &message.data,
            )
            .await?;
        }
        ClientMessage::DeviceToUserDataResponseV1(message) => {
            handle_phone_service_message(
                state,
                context,
                crate::message::wire_id::DEVICE_TO_USER_DATA_RESPONSE_V1,
                PhoneServiceMessageKind::Response,
                PhoneServiceRouting {
                    application_id: ApplicationId::new(message.application_id),
                    line_instance: LineInstance::new(message.line_instance),
                    call_reference: CallReference::new(message.call_reference),
                    transaction_id: TransactionId::new(message.transaction_id),
                },
                Some(PhoneServiceExtendedRouting {
                    sequence_flag: message.sequence_flag,
                    display_priority: message.display_priority,
                    conference_id: message.conference_id,
                    application_instance_id: message.application_instance_id,
                    routing: message.routing,
                }),
                &message.data,
            )
            .await?;
        }
        ClientMessage::OpenMultimediaReceiveChannelAck(ack) => {
            let Some(call_id) = state.calls_by_wire.get(&ack.call_reference.get()).copied() else {
                debug!(device_id = %state.device.id, "ignored video receive acknowledgement for an unknown call");
                return Ok(());
            };
            let Some((request, codec, requested_address_type)) =
                state.calls_by_id.get(&call_id).and_then(|call| {
                    call.video_receive.leg.as_ref().and_then(|leg| {
                        (leg.state == MediaChannelState::Opening
                            && leg.request.token().get() == ack.passthrough_party_id.get())
                        .then_some((leg.request, leg.codec, leg.requested_address_type))
                    })
                })
            else {
                debug!(device_id = %state.device.id, ?call_id, "ignored stale video receive acknowledgement");
                return Ok(());
            };

            let event = if ack.status == MediaStatus::Ok {
                if !endpoint_is_usable(ack.endpoint)
                    || !address_matches_type(ack.endpoint.address, requested_address_type)
                {
                    debug!(device_id = %state.device.id, ?call_id, "ignored unusable video receive endpoint");
                    return Ok(());
                }
                let leg = state
                    .calls_by_id
                    .get_mut(&call_id)
                    .and_then(|call| call.video_receive.leg.as_mut())
                    .expect("correlated video receive leg remains present");
                debug_assert_eq!(leg.request, request);
                leg.state = MediaChannelState::Open;
                leg.deadline = None;
                DeviceEventKind::MultimediaReceiveChannelOpened {
                    call_id,
                    codec,
                    endpoint: ack.endpoint,
                    passthrough_party_id: ack.passthrough_party_id,
                }
            } else {
                let close = take_multimedia_receive_close(state, call_id)
                    .expect("correlated video receive leg remains present");
                send_message(stream, &close, protocol).await?;
                DeviceEventKind::MultimediaReceiveChannelFailed {
                    call_id,
                    codec,
                    status: ack.status,
                    endpoint: ack.endpoint,
                    passthrough_party_id: ack.passthrough_party_id,
                }
            };
            context
                .event_tx
                .send(Event::device(
                    state.device.id.clone(),
                    state.generation,
                    event,
                ))
                .await
                .map_err(|_| ServerError::Stopped)?;
        }
        ClientMessage::StartMultimediaTransmissionAck(ack) => {
            let Some(call_id) = state.calls_by_wire.get(&ack.call_reference.get()).copied() else {
                debug!(device_id = %state.device.id, "ignored video transmit acknowledgement for an unknown call");
                return Ok(());
            };
            let Some((request, codec, address_type)) =
                state.calls_by_id.get(&call_id).and_then(|call| {
                    call.video_transmit.leg.as_ref().and_then(|leg| {
                        (leg.state == MediaChannelState::Opening
                            && leg.request.token().get() == ack.passthrough_party_id.get()
                            && leg.conference_id == ack.conference_id)
                            .then_some((leg.request, leg.codec, leg.address_type))
                    })
                })
            else {
                debug!(device_id = %state.device.id, ?call_id, "ignored stale video transmit acknowledgement");
                return Ok(());
            };

            let event = if ack.status == MediaStatus::Ok {
                if !endpoint_is_usable(ack.endpoint)
                    || !address_matches_type(ack.endpoint.address, address_type)
                {
                    debug!(device_id = %state.device.id, ?call_id, "ignored unusable video transmit endpoint");
                    return Ok(());
                }
                let leg = state
                    .calls_by_id
                    .get_mut(&call_id)
                    .and_then(|call| call.video_transmit.leg.as_mut())
                    .expect("correlated video transmit leg remains present");
                debug_assert_eq!(leg.request, request);
                leg.state = MediaChannelState::Open;
                leg.deadline = None;
                DeviceEventKind::MultimediaTransmitStarted {
                    call_id,
                    codec,
                    endpoint: ack.endpoint,
                    passthrough_party_id: ack.passthrough_party_id,
                }
            } else {
                let stop = take_multimedia_transmit_stop(state, call_id)
                    .expect("correlated video transmit leg remains present");
                send_message(stream, &stop, protocol).await?;
                DeviceEventKind::MultimediaTransmitFailed {
                    call_id,
                    codec,
                    status: ack.status,
                    endpoint: ack.endpoint,
                    passthrough_party_id: ack.passthrough_party_id,
                }
            };
            context
                .event_tx
                .send(Event::device(
                    state.device.id.clone(),
                    state.generation,
                    event,
                ))
                .await
                .map_err(|_| ServerError::Stopped)?;
        }
        message @ (ClientMessage::MediaPortList(_)
        | ClientMessage::SpcpRegisterToken(_)
        | ClientMessage::ExtensionDeviceCapabilities(_)
        | ClientMessage::CreateConferenceResponse(_)
        | ClientMessage::DeleteConferenceResponse { .. }
        | ClientMessage::ModifyConferenceResponse(_)
        | ClientMessage::AuditConferenceResponse(_)
        | ClientMessage::AddParticipantResponse(_)
        | ClientMessage::AuditParticipantResponse(_)) => {
            debug!(device_id = %state.device.id, message = ?message, "deferred SCCP application message");
            context
                .event_tx
                .send(Event::device(
                    state.device.id.clone(),
                    state.generation,
                    DeviceEventKind::UnhandledMessage { message },
                ))
                .await
                .map_err(|_| ServerError::Stopped)?;
        }
        ClientMessage::KnownOpaque(message) => {
            let message = ClientMessage::KnownOpaque(message);
            debug!(device_id = %state.device.id, message = ?message, "unhandled SCCP message");
            context
                .event_tx
                .send(Event::device(
                    state.device.id.clone(),
                    state.generation,
                    DeviceEventKind::UnhandledMessage { message },
                ))
                .await
                .map_err(|_| ServerError::Stopped)?;
        }
        ClientMessage::Unknown(message) => {
            let message = ClientMessage::Unknown(message);
            warn!(device_id = %state.device.id, message = ?message, "unknown SCCP message");
            context
                .event_tx
                .send(Event::device(
                    state.device.id.clone(),
                    state.generation,
                    DeviceEventKind::UnhandledMessage { message },
                ))
                .await
                .map_err(|_| ServerError::Stopped)?;
        }
        ClientMessage::Register(_) | ClientMessage::RegisterToken(_) => {
            warn!(device_id = %state.device.id, "ignoring registration message on registered session");
        }
    }
    Ok(())
}

const fn is_local_audio_path(path: crate::message::values::MediaPathId) -> bool {
    matches!(
        path,
        crate::message::values::MediaPathId::Headset
            | crate::message::values::MediaPathId::Handset
            | crate::message::values::MediaPathId::Speaker
    )
}

fn has_active_media_path(state: &SessionState) -> bool {
    state.media_path_states.iter().any(|(path, event)| {
        is_local_audio_path(*path) && *event == crate::message::values::MediaPathEvent::On
    })
}

fn active_media_path_call(state: &SessionState) -> Option<CallId> {
    state.active_call_id.filter(|call_id| {
        state.calls_by_id.get(call_id).is_some_and(|call| {
            !matches!(
                call.state,
                CallState::OnHook
                    | CallState::RingIn
                    | CallState::CallWaiting
                    | CallState::Hold
                    | CallState::HoldYellow
                    | CallState::HoldRed
            )
        })
    })
}

async fn complete_on_hook(
    stream: &mut dyn StationIo,
    state: &mut SessionState,
    context: &SessionContext,
    call: SessionCall,
    line_instance: u32,
) -> Result<(), ServerError> {
    state.pending_media_path_release = None;
    let order = *context
        .call_answer_order
        .read()
        .expect("SCCP call-answer-order lock poisoned");
    let successor = incoming_successor(state, call.call_id, order);
    let successor_has_ringer = successor.is_some_and(|(call_id, _)| {
        state
            .calls_by_id
            .get(&call_id)
            .and_then(|call| incoming_ringer(call.ringer, CallState::RingIn))
            .is_some_and(ringer_is_audible)
    });
    let stop_ringer =
        !successor_has_ringer && state.ringer_owner.is_none_or(|owner| owner == call.call_id);
    context
        .event_tx
        .send(Event::device(
            state.device.id.clone(),
            state.generation,
            DeviceEventKind::OnHook {
                call_id: call.call_id,
                line_instance: LineInstance::new(line_instance),
            },
        ))
        .await
        .map_err(|_| ServerError::Stopped)?;
    state.active_key_mode = KeyMode::OnHook;
    stop_call_multicast(stream, state, call.call_id, state.registration.protocol).await?;
    close_call_media_messages(stream, &call, state.registration.protocol).await?;
    close_call_messages(
        stream,
        &call,
        &state.device.soft_keys,
        state.registration.protocol,
        context.config.timezone_offset_minutes,
        stop_ringer,
    )
    .await?;
    request_connection_statistics(stream, state, &call, context).await?;
    if let Some(stored) = state.calls_by_id.get_mut(&call.call_id) {
        stored.state = CallState::OnHook;
        stored.media.receive.state = MediaChannelState::Closed;
        stored.media.receive.deadline = None;
        stored.media.transmit.state = MediaChannelState::Closed;
        stored.media.transmit.deadline = None;
        stored.media.transmit_confirmation = TransmitConfirmation::Inactive;
        stored.media.coupled_transmit_endpoint = None;
        stored.video_receive.leg = None;
        stored.video_transmit.leg = None;
    }
    if state.active_call_id == Some(call.call_id) {
        state.active_call_id = None;
    }
    if state.ringer_owner == Some(call.call_id) {
        state.ringer_owner = None;
    }
    if let Some((call_id, promote)) = successor {
        present_incoming_successor(stream, state, call_id, promote).await?;
    }
    Ok(())
}

#[cfg(test)]
fn button_template(device: &DeviceDefinition) -> Vec<ButtonTemplateEntry> {
    button_template_for_station(device, ProtocolVersion::V15, DeviceType::Undefined)
}

fn recording_uses_multiblink(protocol: ProtocolVersion, device_type: DeviceType) -> bool {
    protocol > ProtocolVersion::V15
        && !matches!(device_type, DeviceType::Cisco8941 | DeviceType::Cisco8945)
}

fn button_template_for_station(
    device: &DeviceDefinition,
    protocol: ProtocolVersion,
    device_type: DeviceType,
) -> Vec<ButtonTemplateEntry> {
    let mut buttons = Vec::with_capacity(56);
    let mut addon_buttons_remaining = None;
    for button in &device.buttons {
        if let ButtonDefinition::AddonModule(addon) = button {
            buttons.extend(std::iter::repeat_n(
                ButtonTemplateEntry {
                    instance: 0,
                    button_type: ButtonType::Unused,
                },
                addon_buttons_remaining.take().unwrap_or_default(),
            ));
            addon_buttons_remaining = addon.button_capacity();
            continue;
        }
        buttons.push(match button {
            ButtonDefinition::Line(appearance) => ButtonTemplateEntry {
                instance: appearance.instance,
                button_type: ButtonType::Line,
            },
            ButtonDefinition::SpeedDial(speed_dial) => ButtonTemplateEntry {
                instance: speed_dial.instance,
                button_type: ButtonType::SpeedDial,
            },
            ButtonDefinition::BlfSpeedDial(speed_dial) => ButtonTemplateEntry {
                instance: speed_dial.instance,
                button_type: ButtonType::BlfSpeedDial,
            },
            ButtonDefinition::Feature(feature) => ButtonTemplateEntry {
                instance: feature.instance,
                button_type: ButtonType::from(feature.feature.wire_value()),
            },
            ButtonDefinition::Recording(recording) => ButtonTemplateEntry {
                instance: recording.instance,
                button_type: if recording_uses_multiblink(protocol, device_type) {
                    ButtonType::MultiblinkFeature
                } else {
                    ButtonType::Feature
                },
            },
            ButtonDefinition::Service(service) => ButtonTemplateEntry {
                instance: service.instance,
                button_type: ButtonType::ServiceUrl,
            },
            ButtonDefinition::Unused => ButtonTemplateEntry {
                instance: 0,
                button_type: ButtonType::Unused,
            },
            ButtonDefinition::AddonModule(_) => unreachable!("addon marker handled above"),
        });
        if let Some(remaining) = &mut addon_buttons_remaining {
            *remaining = remaining.saturating_sub(1);
        }
    }
    buttons.extend(std::iter::repeat_n(
        ButtonTemplateEntry {
            instance: 0,
            button_type: ButtonType::Unused,
        },
        addon_buttons_remaining.unwrap_or_default(),
    ));
    buttons
}

async fn send_button_template(
    stream: &mut dyn StationIo,
    device: &DeviceDefinition,
    session: impl Into<StationSessionContext>,
    device_type: DeviceType,
) -> Result<(), ServerError> {
    let session = session.into();
    for message in button_template_messages_for_station(device, session.protocol, device_type)? {
        send_message(stream, &message, session).await?;
    }
    Ok(())
}

#[cfg(test)]
fn button_template_messages(device: &DeviceDefinition) -> Result<Vec<ServerMessage>, CodecError> {
    button_template_messages_for_station(device, ProtocolVersion::V15, DeviceType::Undefined)
}

fn button_template_messages_for_station(
    device: &DeviceDefinition,
    protocol: ProtocolVersion,
    device_type: DeviceType,
) -> Result<Vec<ServerMessage>, CodecError> {
    let buttons = button_template_for_station(device, protocol, device_type);
    let total = u32::try_from(buttons.len()).map_err(|_| {
        CodecError::InvalidDefinition(format!(
            "device {} button template is too large for SCCP",
            device.id
        ))
    })?;
    if buttons.is_empty() {
        return Ok(vec![ServerMessage::ButtonTemplate {
            offset: 0,
            total: 0,
            buttons: Vec::new(),
        }]);
    }
    Ok(buttons
        .chunks(BUTTON_TEMPLATE_ENTRIES_PER_CHUNK)
        .enumerate()
        .map(|(chunk_index, chunk)| ServerMessage::ButtonTemplate {
            offset: u32::try_from(chunk_index * BUTTON_TEMPLATE_ENTRIES_PER_CHUNK)
                .expect("validated button template offset"),
            total,
            buttons: chunk.to_vec(),
        })
        .collect())
}

fn line_status(device: &DeviceDefinition, instance: u32) -> Option<ServerMessage> {
    let appearance = device.line(instance)?;
    let is_primary = device
        .first_line()
        .is_some_and(|primary| primary.id == appearance.id);
    let fully_qualified_display_name = if is_primary && !device.description.is_empty() {
        device.description.clone()
    } else {
        appearance.line.number.clone()
    };
    Some(ServerMessage::LineStatus {
        instance: appearance.instance,
        directory_number: appearance.line.number.clone(),
        fully_qualified_display_name,
        display_label: appearance.display_label().to_owned(),
    })
}

fn call_count_response(device: &DeviceDefinition) -> Result<ServerMessage, CodecError> {
    let lines = device.lines().collect::<Vec<_>>();
    let total_configured_lines = u32::try_from(lines.len()).map_err(|_| {
        CodecError::InvalidDefinition(format!(
            "device {} has too many lines for a call-count response",
            device.id
        ))
    })?;
    let starting_line_instance = lines.first().map_or(0, |line| line.instance);
    let line_data = lines
        .into_iter()
        .take(CALL_COUNT_RESPONSE_MAX_LINE_ENTRIES)
        .map(|_| CallCountLineData {
            max_calls: DEFAULT_MAX_CALLS_PER_LINE,
            busy_trigger: DEFAULT_BUSY_TRIGGER_PER_LINE,
        })
        .collect();

    Ok(ServerMessage::CallCountResponse(CallCountResponse {
        total_configured_lines,
        starting_line_instance,
        line_data,
    }))
}

fn mobility_device_candidate(
    current: &DeviceDefinition,
    current_appearances: &HashMap<u32, LineAppearance>,
    next_appearances: &HashMap<u32, LineAppearance>,
) -> Result<DeviceDefinition, CodecError> {
    let mut candidate = current.clone();
    candidate.buttons.retain(|button| {
        !matches!(
            button,
            ButtonDefinition::Line(line)
                if current_appearances.values().any(|appearance| appearance == line)
        )
    });
    let mut index = 0;
    while index < candidate.buttons.len() {
        let mobility_instance = match &candidate.buttons[index] {
            ButtonDefinition::Feature(feature) if feature.feature == ButtonType::Mobility => {
                Some(feature.instance)
            }
            _ => None,
        };
        if let Some(appearance) = mobility_instance
            .and_then(|instance| next_appearances.get(&instance))
            .cloned()
        {
            candidate
                .buttons
                .insert(index + 1, ButtonDefinition::Line(appearance));
            index += 2;
        } else {
            index += 1;
        }
    }
    candidate.validate()?;
    Ok(candidate)
}

fn speed_dial_status(device: &DeviceDefinition, instance: u32) -> ServerMessage {
    let speed_dial = device.buttons.iter().find_map(|button| match button {
        ButtonDefinition::SpeedDial(speed_dial) if speed_dial.instance == instance => {
            Some((&speed_dial.number, &speed_dial.display_name))
        }
        _ => None,
    });
    ServerMessage::SpeedDialStatus {
        instance,
        number: speed_dial.map_or_else(String::new, |(number, _)| number.clone()),
        display_name: speed_dial.map_or_else(String::new, |(_, display_name)| display_name.clone()),
    }
}

#[cfg(test)]
fn feature_status(
    device: &DeviceDefinition,
    instance: u32,
    capabilities: u32,
) -> Option<ServerMessage> {
    feature_status_for_station(
        device,
        instance,
        capabilities,
        ProtocolVersion::V15,
        DeviceType::Undefined,
        PhoneFeatures::empty(),
    )
}

fn feature_status_for_station(
    device: &DeviceDefinition,
    instance: u32,
    _capabilities: u32,
    protocol: ProtocolVersion,
    device_type: DeviceType,
    features: PhoneFeatures,
) -> Option<ServerMessage> {
    if let Some(speed_dial) = device.blf_button(instance) {
        return Some(ServerMessage::FeatureStatus {
            instance,
            button_type: ButtonType::BlfSpeedDial,
            label: speed_dial.display_name.clone(),
            state: BusyLampFieldState::UnknownState.wire_value(),
        });
    }
    if let Some(recording) = device.recording_button(instance) {
        return Some(ServerMessage::FeatureStatus {
            instance,
            button_type: if recording_uses_multiblink(protocol, device_type) {
                ButtonType::MultiblinkFeature
            } else {
                ButtonType::Feature
            },
            label: recording_button_label(&recording.label, RecordingButtonState::Off, features),
            state: recording_button_status_word(RecordingButtonState::Off, protocol, device_type),
        });
    }
    device
        .feature_button(instance)
        .map(|feature| ServerMessage::FeatureStatus {
            instance,
            button_type: ButtonType::from(feature.feature.wire_value()),
            label: feature.label.clone(),
            state: 0,
        })
}

fn feature_state_messages(
    device: &DeviceDefinition,
    instance: u32,
    enabled: bool,
) -> Option<[ServerMessage; 2]> {
    let feature = device.feature_button(instance)?;
    Some([
        ServerMessage::FeatureStatus {
            instance,
            button_type: ButtonType::from(feature.feature.wire_value()),
            label: feature.label.clone(),
            state: u32::from(enabled),
        },
        ServerMessage::SetLamp {
            stimulus: feature.feature,
            instance,
            mode: if enabled { LampMode::On } else { LampMode::Off },
        },
    ])
}

fn cache_feature_projection(
    cache: &mut HashMap<u32, SessionFeatureState>,
    instance: u32,
    message: &ServerMessage,
) {
    let ServerMessage::FeatureStatus {
        button_type,
        label,
        state,
        ..
    } = message
    else {
        debug_assert!(false, "feature projection cache requires FeatureStatus");
        return;
    };
    cache.insert(
        instance,
        SessionFeatureState {
            button_type: *button_type,
            label: label.clone(),
            state: *state,
        },
    );
}

fn apply_cached_feature_projection(
    cache: &HashMap<u32, SessionFeatureState>,
    instance: u32,
    message: &mut ServerMessage,
) {
    let Some(cached) = cache.get(&instance) else {
        return;
    };
    if let ServerMessage::FeatureStatus {
        button_type,
        label,
        state,
        ..
    } = message
    {
        *button_type = cached.button_type;
        label.clone_from(&cached.label);
        *state = cached.state;
    }
}

fn recording_button_status_word(
    state: RecordingButtonState,
    protocol: ProtocolVersion,
    device_type: DeviceType,
) -> u32 {
    const ARMED_STATUS_WORD: u32 = 0x02_03_02;
    const ACTIVE_STATUS_WORD: u32 = 0x03_02_03;
    const ARMED_ACTIVE_STATUS_WORD: u32 = 0x03_02_05;

    if !recording_uses_multiblink(protocol, device_type) {
        return (state.is_armed() || state.is_active()) as u32;
    }
    match state {
        RecordingButtonState::Off => 0,
        RecordingButtonState::Armed => ARMED_STATUS_WORD,
        RecordingButtonState::Active => ACTIVE_STATUS_WORD,
        RecordingButtonState::ArmedActive => ARMED_ACTIVE_STATUS_WORD,
    }
}

fn recording_button_label(
    configured: &str,
    state: RecordingButtonState,
    features: PhoneFeatures,
) -> String {
    const ACTIVE_SUFFIX: &str = " (Recording)";
    const MAX_DYNAMIC_FEATURE_LABEL_BYTES: usize = 120;
    if !state.is_active() {
        return configured.to_owned();
    }
    let capacity = if features.contains(PhoneFeatures::DYNAMIC_MESSAGES) {
        MAX_DYNAMIC_FEATURE_LABEL_BYTES
    } else {
        crate::types::MAX_STATION_FEATURE_LABEL_BYTES
    };
    let encoded_len = |text: &str| {
        if features.contains(PhoneFeatures::UTF8) {
            text.len()
        } else {
            text.chars().count()
        }
    };
    if encoded_len(configured) + ACTIVE_SUFFIX.len() <= capacity {
        format!("{configured}{ACTIVE_SUFFIX}")
    } else {
        configured.to_owned()
    }
}

fn recording_button_state_messages(
    device: &DeviceDefinition,
    instance: u32,
    state: RecordingButtonState,
    protocol: ProtocolVersion,
    device_type: DeviceType,
    features: PhoneFeatures,
) -> Option<[ServerMessage; 2]> {
    let recording = device.recording_button(instance)?;
    let button_type = if recording_uses_multiblink(protocol, device_type) {
        ButtonType::MultiblinkFeature
    } else {
        ButtonType::Feature
    };
    let lamp_mode = match state {
        RecordingButtonState::Off => LampMode::Off,
        RecordingButtonState::Armed => LampMode::On,
        RecordingButtonState::Active => LampMode::Wink,
        RecordingButtonState::ArmedActive => LampMode::Blink,
    };
    Some([
        ServerMessage::FeatureStatus {
            instance,
            button_type,
            label: recording_button_label(&recording.label, state, features),
            state: recording_button_status_word(state, protocol, device_type),
        },
        ServerMessage::SetLamp {
            stimulus: button_type,
            instance,
            mode: lamp_mode,
        },
    ])
}

/// Pack the three DND multiblink selectors used by v16+ stations.
///
/// The low byte selects the primary icon state, the middle byte the lamp
/// cadence, and the high byte the alternate state. Cisco's canonical words
/// are intentionally preserved rather than recomputed from boolean state.
const fn multiblink_dnd_state(mode: DoNotDisturbMode) -> u32 {
    const OFF: u32 = 0x01_00_00;
    const REJECT: u32 = 0x02_02_02;
    const SILENT: u32 = 0x03_03_02;

    match mode {
        DoNotDisturbMode::Off => OFF,
        DoNotDisturbMode::Reject => REJECT,
        DoNotDisturbMode::Silent => SILENT,
    }
}

fn do_not_disturb_state_messages(
    device: &DeviceDefinition,
    instance: u32,
    mode: DoNotDisturbMode,
    button_mode: DoNotDisturbButtonMode,
    protocol: ProtocolVersion,
) -> Option<[ServerMessage; 2]> {
    let feature = device
        .feature_button(instance)
        .filter(|feature| feature.feature == ButtonType::DoNotDisturb)?;
    let exact_enabled = match button_mode {
        DoNotDisturbButtonMode::Cycle => mode != DoNotDisturbMode::Off,
        DoNotDisturbButtonMode::Silent => mode == DoNotDisturbMode::Silent,
        DoNotDisturbButtonMode::Reject => mode == DoNotDisturbMode::Reject,
    };
    let multi_state =
        button_mode == DoNotDisturbButtonMode::Cycle && protocol > ProtocolVersion::V15;
    let (button_type, state) = if multi_state {
        (ButtonType::MultiblinkFeature, multiblink_dnd_state(mode))
    } else {
        (ButtonType::DoNotDisturb, u32::from(exact_enabled))
    };
    let lamp = match (exact_enabled, mode) {
        (false, _) | (_, DoNotDisturbMode::Off) => LampMode::Off,
        (true, DoNotDisturbMode::Silent) => LampMode::Blink,
        (true, DoNotDisturbMode::Reject) => LampMode::On,
    };
    Some([
        ServerMessage::FeatureStatus {
            instance,
            button_type,
            label: feature.label.clone(),
            state,
        },
        ServerMessage::SetLamp {
            stimulus: feature.feature,
            instance,
            mode: lamp,
        },
    ])
}

fn blf_status_message(
    device: &DeviceDefinition,
    instance: u32,
    state: BlfState,
) -> Option<ServerMessage> {
    let definition = device.blf_button(instance)?;
    let icon = match state {
        BlfState::Idle => BusyLampFieldState::Idle,
        BlfState::Ringing => BusyLampFieldState::Alerting,
        BlfState::Busy | BlfState::Held => BusyLampFieldState::InUse,
        BlfState::DoNotDisturb => BusyLampFieldState::DoNotDisturb,
        BlfState::Unavailable | BlfState::Unknown => BusyLampFieldState::UnknownState,
    };
    Some(ServerMessage::FeatureStatus {
        instance,
        button_type: ButtonType::BlfSpeedDial,
        label: definition.display_name.clone(),
        state: icon.wire_value(),
    })
}

fn hinted_ringing_notification(
    device: &DeviceDefinition,
    label: &str,
    caller: Option<&BlfCallerInfo>,
    state: BlfState,
) -> Option<HandsetStatusMessage> {
    if !device.ui.hinted_ringing_notification || state != BlfState::Ringing {
        return None;
    }
    let caller = caller.map(BlfCallerInfo::display).unwrap_or_default();
    let text = if caller.is_empty() {
        format!("{label} is ringing")
    } else {
        format!("{label} is ringing: {caller}")
    };
    Some(HandsetStatusMessage::Display {
        text: truncate_utf8(&text, 79),
        timeout_seconds: 5,
        priority: None,
    })
}

fn reconcile_blf_alert(
    instance: u32,
    notification: Option<HandsetStatusMessage>,
    active: &mut BTreeMap<u32, HandsetStatusMessage>,
    visible: &mut Option<HandsetStatusMessage>,
) -> Option<HandsetStatusMessage> {
    match notification {
        Some(notification) => {
            active.insert(instance, notification);
        }
        None => {
            active.remove(&instance);
        }
    }
    let next = active.first_key_value().map(|(_, message)| message.clone());
    if *visible == next {
        return None;
    }
    *visible = next.clone();
    Some(next.unwrap_or(HandsetStatusMessage::Clear { priority: None }))
}

fn truncate_utf8(value: &str, maximum_bytes: usize) -> String {
    if value.len() <= maximum_bytes {
        return value.to_owned();
    }
    let end = value
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= maximum_bytes)
        .last()
        .unwrap_or(0);
    value[..end].to_owned()
}

fn service_url_status(device: &DeviceDefinition, index: u32) -> Option<ServerMessage> {
    device.buttons.iter().find_map(|button| match button {
        ButtonDefinition::Service(service) if service.instance == index => {
            Some(ServerMessage::ServiceUrlStatus {
                index,
                url: service.url.clone(),
                label: service.label.clone(),
                extension_text: String::new(),
            })
        }
        _ => None,
    })
}

const fn key_mode_for_call_state(state: CallState) -> KeyMode {
    match state {
        CallState::Connected => KeyMode::Connected,
        CallState::Hold | CallState::HoldYellow | CallState::HoldRed => KeyMode::OnHold,
        CallState::RingIn | CallState::CallWaiting => KeyMode::RingIn,
        CallState::OffHook
        | CallState::Busy
        | CallState::Congestion
        | CallState::InvalidNumber
        | CallState::IntercomOneWay => KeyMode::OffHook,
        CallState::Transfer => KeyMode::ConnectedTransfer,
        CallState::RingOut | CallState::Proceed => KeyMode::RingOut,
        CallState::RemoteMultiline => KeyMode::OnHookStealable,
        CallState::OnHook | CallState::Park | CallState::Unknown(_) => KeyMode::OnHook,
    }
}

fn transfer_key_mode(call: &SessionCall, state: CallState) -> KeyMode {
    if matches!(
        call.transfer_role,
        Some(SessionTransferRole::Consultation { .. })
    ) && matches!(state, CallState::RingOut | CallState::Connected)
    {
        KeyMode::ConnectedTransfer
    } else {
        key_mode_for_call_state(state)
    }
}

fn stimulus_soft_key(stimulus: Stimulus) -> Option<SoftKey> {
    Some(match stimulus {
        Stimulus::LastNumberRedial => SoftKey::Redial,
        Stimulus::Hold => SoftKey::Hold,
        Stimulus::Transfer => SoftKey::Transfer,
        Stimulus::ForwardAll => SoftKey::ForwardAll,
        Stimulus::ForwardBusy => SoftKey::ForwardBusy,
        Stimulus::ForwardNoAnswer => SoftKey::ForwardNoAnswer,
        Stimulus::Conference => SoftKey::Conference,
        Stimulus::MeetMeConference => SoftKey::MeetMe,
        Stimulus::CallPark => SoftKey::Park,
        Stimulus::CallPickup => SoftKey::Pickup,
        Stimulus::GroupCallPickup => SoftKey::GroupPickup,
        Stimulus::DoNotDisturb => SoftKey::DoNotDisturb,
        Stimulus::ConferenceList => SoftKey::ConferenceList,
        Stimulus::NewCall => SoftKey::NewCall,
        Stimulus::EndCall => SoftKey::EndCall,
        _ => return None,
    })
}

fn parking_menu_xml(
    instance: u32,
    transaction_id: u32,
    lot: &str,
    calls: &[ParkingMenuEntry],
) -> Result<String, ServerError> {
    if calls.len() > PARKING_MENU_MAX_ITEMS {
        return Err(PhoneXmlError::LimitExceeded {
            kind: "parking menu",
            actual: calls.len(),
            maximum: PARKING_MENU_MAX_ITEMS,
        }
        .into());
    }
    let items = calls
        .iter()
        .map(|call| {
            let party = if !call.caller_name.trim().is_empty() {
                call.caller_name.trim()
            } else if !call.caller_number.trim().is_empty() {
                call.caller_number.trim()
            } else {
                "Unknown caller"
            };
            let connected = if !call.connected_name.trim().is_empty() {
                format!(" to {}", call.connected_name.trim())
            } else if !call.connected_number.trim().is_empty() {
                format!(" to {}", call.connected_number.trim())
            } else {
                String::new()
            };
            CiscoIpPhoneMenuItem {
                name: Some(format!("{}: {}{}", call.slot, party, connected)),
                url: Some(format!(
                    "UserCallData:{}:{instance}:0:{transaction_id}:retrieve/{}/{}",
                    PARKING_APPLICATION_ID,
                    utf8_percent_encode(lot, NON_ALPHANUMERIC),
                    call.slot,
                )),
            }
        })
        .collect();
    CiscoIpPhoneMenu::new(
        format!("Parked calls - {lot}"),
        if calls.is_empty() {
            "No parked calls"
        } else {
            "Select a call"
        },
        items,
    )?
    .to_xml_with_limit(2_000)
    .map_err(ServerError::from)
}

fn text_service_messages(
    line_instance: LineInstance,
    call_reference: CallReference,
    transaction_id: TransactionId,
    priority: PhoneServicePriority,
    document: &CiscoIpPhoneText,
    protocol: ProtocolVersion,
) -> Result<Vec<ServerMessage>, ServerError> {
    if protocol <= ProtocolVersion::V17
        && document
            .text
            .as_deref()
            .is_some_and(|text| text.chars().count() > PHONE_TEXT_LEGACY_MAX_CHARS)
    {
        return Err(PhoneXmlError::InvalidField {
            field: "legacy phone text body",
            expected: "at most 1024 characters",
        }
        .into());
    }
    let maximum_bytes = if protocol <= ProtocolVersion::V17 {
        2_000
    } else {
        crate::phone::xml::PHONE_TEXT_MAX_BYTES
    };
    let xml = document.to_xml_with_limit(maximum_bytes)?.into_bytes();
    Ok(phone_service_document_messages(
        line_instance,
        call_reference,
        ApplicationId::new(PHONE_TEXT_APPLICATION_ID),
        transaction_id,
        priority,
        &xml,
    ))
}

fn input_service_messages(
    line_instance: LineInstance,
    call_reference: CallReference,
    application_id: ApplicationId,
    transaction_id: TransactionId,
    priority: PhoneServicePriority,
    document: &CiscoIpPhoneInput,
    protocol: ProtocolVersion,
) -> Result<Vec<ServerMessage>, ServerError> {
    let maximum_bytes = if protocol <= ProtocolVersion::V17 {
        2_000
    } else {
        PHONE_INPUT_MAX_BYTES
    };
    let xml = document.to_xml_with_limit(maximum_bytes)?.into_bytes();
    Ok(phone_service_document_messages(
        line_instance,
        call_reference,
        application_id,
        transaction_id,
        priority,
        &xml,
    ))
}

fn execute_phone_action_messages(
    line_instance: LineInstance,
    call_reference: CallReference,
    application_id: ApplicationId,
    transaction_id: TransactionId,
    priority: PhoneServicePriority,
    document: &CiscoIpPhoneExecute,
    protocol: ProtocolVersion,
) -> Result<Vec<ServerMessage>, ServerError> {
    let maximum_bytes = if protocol <= ProtocolVersion::V17 {
        2_000
    } else {
        PHONE_EXECUTE_MAX_BYTES
    };
    let xml = document.to_xml_with_limit(maximum_bytes)?.into_bytes();
    Ok(phone_service_document_messages(
        line_instance,
        call_reference,
        application_id,
        transaction_id,
        priority,
        &xml,
    ))
}

fn image_service_messages(
    line_instance: LineInstance,
    call_reference: CallReference,
    application_id: ApplicationId,
    transaction_id: TransactionId,
    priority: PhoneServicePriority,
    document: &PhoneImageDocument,
    protocol: ProtocolVersion,
) -> Result<Vec<ServerMessage>, ServerError> {
    let maximum_bytes = if protocol <= ProtocolVersion::V17 {
        2_000
    } else {
        PHONE_IMAGE_MAX_BYTES
    };
    let xml = document.to_xml_with_limit(maximum_bytes)?.into_bytes();
    Ok(phone_service_document_messages(
        line_instance,
        call_reference,
        application_id,
        transaction_id,
        priority,
        &xml,
    ))
}

fn status_service_messages(
    line_instance: LineInstance,
    call_reference: CallReference,
    application_id: ApplicationId,
    transaction_id: TransactionId,
    priority: PhoneServicePriority,
    document: &PhoneStatusDocument,
    protocol: ProtocolVersion,
) -> Result<Vec<ServerMessage>, ServerError> {
    let maximum_bytes = if protocol <= ProtocolVersion::V17 {
        2_000
    } else {
        PHONE_STATUS_MAX_BYTES
    };
    let xml = document.to_xml_with_limit(maximum_bytes)?.into_bytes();
    Ok(phone_service_document_messages(
        line_instance,
        call_reference,
        application_id,
        transaction_id,
        priority,
        &xml,
    ))
}

fn background_control_message(
    transaction_id: TransactionId,
    document: &PhoneBackgroundControlDocument,
) -> Result<ServerMessage, ServerError> {
    let xml = document.to_xml()?.into_bytes();
    let [message] = phone_service_document_messages(
        LineInstance::new(0),
        CallReference::new(0),
        ApplicationId::new(PHONE_BACKGROUND_APPLICATION_ID),
        transaction_id,
        PhoneServicePriority::LOW,
        &xml,
    )
    .try_into()
    .map_err(|_| PhoneXmlError::InvalidField {
        field: "background control document",
        expected: "a single application-data frame",
    })?;
    Ok(message)
}

fn ringtone_control_message(
    transaction_id: TransactionId,
    document: &CiscoIpPhoneSetRingTone,
) -> Result<ServerMessage, ServerError> {
    let xml = document.to_xml()?.into_bytes();
    let [message] = phone_service_document_messages(
        LineInstance::new(0),
        CallReference::new(0),
        ApplicationId::new(PHONE_RINGTONE_APPLICATION_ID),
        transaction_id,
        PhoneServicePriority::LOW,
        &xml,
    )
    .try_into()
    .map_err(|_| PhoneXmlError::InvalidField {
        field: "ringtone control document",
        expected: "a single application-data frame",
    })?;
    Ok(message)
}

#[cfg(test)]
fn start_announcement_message(
    conference_id: ConferenceId,
    announcements: Vec<AnnouncementEntry>,
    end_of_ack: bool,
    participant_ids: Vec<ParticipantId>,
    hearing_participant_mask: u32,
    play_mode: u32,
) -> ServerMessage {
    ServerMessage::StartAnnouncement {
        announcements,
        end_of_ack: u32::from(end_of_ack),
        conference_id: conference_id.get(),
        matrix_conference_party_ids: participant_ids
            .into_iter()
            .map(ParticipantId::get)
            .collect(),
        hearing_conference_party_mask: hearing_participant_mask,
        play_mode,
    }
}

fn phone_service_document_messages(
    line_instance: LineInstance,
    call_reference: CallReference,
    application_id: ApplicationId,
    transaction_id: TransactionId,
    priority: PhoneServicePriority,
    xml: &[u8],
) -> Vec<ServerMessage> {
    let chunks = xml.chunks(2_000);
    let chunk_count = chunks.len();
    chunks
        .enumerate()
        .map(|(index, data)| {
            let sequence_flag = if chunk_count == 1 || index + 1 == chunk_count {
                2
            } else if index == 0 {
                0
            } else {
                1
            };
            ServerMessage::UserToDeviceDataV1(UserDataV1Message {
                application_id: application_id.get(),
                line_instance: line_instance.get(),
                call_reference: call_reference.get(),
                transaction_id: transaction_id.get(),
                sequence_flag,
                display_priority: priority.wire(),
                conference_id: call_reference.get(),
                application_instance_id: application_id.get(),
                routing: 1,
                data: data.to_vec(),
            })
        })
        .collect()
}

async fn handle_phone_service_message(
    state: &mut SessionState,
    context: &SessionContext,
    message_id: u32,
    kind: PhoneServiceMessageKind,
    routing: PhoneServiceRouting,
    extended: Option<PhoneServiceExtendedRouting>,
    data: &[u8],
) -> Result<(), ServerError> {
    let payload = match parse_phone_service_payload(data, kind) {
        Ok(payload) => payload,
        Err(error) => {
            warn!(
                device_id = %state.device.id,
                message_id = format_args!("0x{message_id:04x}"),
                %error,
                "ignoring malformed phone-service response"
            );
            context
                .event_tx
                .send(Event::ProtocolWarning {
                    peer: context.peer,
                    device_id: Some(state.device.id.clone()),
                    message_id,
                    error: error.to_string(),
                })
                .await
                .map_err(|_| ServerError::Stopped)?;
            return Ok(());
        }
    };
    let response = PhoneServiceEvent {
        kind,
        routing,
        extended,
        payload,
    };

    if let Some((lot, slot)) = parking_menu_selection(state.pending_parking_menu, &response) {
        state.pending_parking_menu = None;
        context
            .event_tx
            .send(Event::device(
                state.device.id.clone(),
                state.generation,
                DeviceEventKind::ParkingMenuSelection { lot, slot },
            ))
            .await
            .map_err(|_| ServerError::Stopped)?;
    }
    if response.kind == PhoneServiceMessageKind::Data
        && response.routing.application_id.get() == ConferenceListAction::APPLICATION_ID
        && let PhoneServicePayload::Submission(submission) = &response.payload
        && let Some(action) = ConferenceListAction::from_route(&submission.route)
    {
        context
            .event_tx
            .send(Event::device(
                state.device.id.clone(),
                state.generation,
                DeviceEventKind::ConferenceListAction { action },
            ))
            .await
            .map_err(|_| ServerError::Stopped)?;
    }
    context
        .event_tx
        .send(Event::device(
            state.device.id.clone(),
            state.generation,
            DeviceEventKind::PhoneServiceResponse { response },
        ))
        .await
        .map_err(|_| ServerError::Stopped)
}

fn parking_menu_selection(
    pending: Option<PendingParkingMenu>,
    response: &PhoneServiceEvent,
) -> Option<(String, u32)> {
    let pending = pending?;
    if response.kind != PhoneServiceMessageKind::Data
        || response.routing.application_id.get() != PARKING_APPLICATION_ID
        || response.routing.line_instance.get() != pending.instance
        || response.routing.call_reference.get() != 0
        || response.routing.transaction_id.get() != pending.transaction_id
        || response
            .extended
            .is_some_and(|extended| extended.application_instance_id != pending.instance)
    {
        return None;
    }
    let PhoneServicePayload::Submission(submission) = &response.payload else {
        return None;
    };
    let [action, lot, slot] = submission.route.as_slice() else {
        return None;
    };
    if action != "retrieve" || lot.is_empty() || !submission.values.is_empty() {
        return None;
    }
    let slot = slot.parse().ok()?;
    (slot != 0).then(|| (lot.clone(), slot))
}

fn digit_character(digit: Digit) -> Option<char> {
    match digit {
        Digit::Number(number @ 0..=9) => Some(char::from(b'0' + number)),
        Digit::Star => Some('*'),
        Digit::Pound => Some('#'),
        Digit::A => Some('A'),
        Digit::B => Some('B'),
        Digit::C => Some('C'),
        Digit::D => Some('D'),
        Digit::Number(_) | Digit::Unknown(_) => None,
    }
}

fn normalized_last_number(number: &str, config: &ServerConfig) -> Option<String> {
    let number = number.trim();
    let number = if config.record_dial_terminator {
        number
    } else {
        digit_character(config.dial_terminator)
            .map_or(number, |terminator| number.trim_end_matches(terminator))
    };
    (!number.is_empty()).then(|| number.to_owned())
}

fn remember_last_number(
    state: &mut SessionState,
    line_instance: u32,
    number: &str,
    config: &ServerConfig,
) {
    if let Some(number) = normalized_last_number(number, config) {
        state.last_number_by_line.insert(line_instance, number);
    }
}

async fn begin_redial(
    stream: &mut dyn StationIo,
    state: &mut SessionState,
    context: &SessionContext,
    line_instance: u32,
    existing_call_id: Option<CallId>,
) -> Result<(), ServerError> {
    if state.device.ui.placed_calls_redial_menu
        && placed_calls_menu_supported(state.registration.protocol)
    {
        let document = CiscoIpPhoneExecute::new(vec![CiscoIpPhoneExecuteItem::new(
            "Application:PlacedCalls",
        )?])?;
        for message in execute_phone_action_messages(
            LineInstance::new(line_instance),
            CallReference::new(0),
            ApplicationId::new(0),
            TransactionId::new(0),
            PhoneServicePriority::NORMAL,
            &document,
            state.registration.protocol,
        )? {
            send_message(stream, &message, state.registration.protocol).await?;
        }
        return Ok(());
    }

    let Some(number) = state.last_number_by_line.get(&line_instance).cloned() else {
        return Ok(());
    };
    let existing = existing_call_id.and_then(|call_id| {
        state
            .calls_by_id
            .get(&call_id)
            .filter(|call| call.line_instance == line_instance && call.state != CallState::OnHook)
            .cloned()
    });
    let (call, created) = existing.map_or_else(
        || {
            (
                ensure_phone_call(state, 0, line_instance, &context.next_call_id),
                true,
            )
        },
        |call| (call, false),
    );

    if created {
        state.active_key_mode = KeyMode::OffHook;
        begin_phone_call_ui(stream, &call, &state.device, state.station_context()).await?;
        context
            .event_tx
            .send(Event::device(
                state.device.id.clone(),
                state.generation,
                DeviceEventKind::OffHook {
                    call_id: call.call_id,
                    line_instance: LineInstance::new(line_instance),
                },
            ))
            .await
            .map_err(|_| ServerError::Stopped)?;
    }
    if let Some(stored) = state.calls_by_id.get_mut(&call.call_id) {
        stored.dialed_number.clone_from(&number);
    }
    send_message(
        stream,
        &ServerMessage::DialedNumber {
            number: number.clone(),
            line_instance,
            call_reference: call.wire_reference,
        },
        state.registration.protocol,
    )
    .await?;
    context
        .event_tx
        .send(Event::device(
            state.device.id.clone(),
            state.generation,
            DeviceEventKind::EnblocCall {
                call_id: call.call_id,
                line_instance: LineInstance::new(line_instance),
                number,
            },
        ))
        .await
        .map_err(|_| ServerError::Stopped)?;
    Ok(())
}

fn placed_calls_menu_supported(protocol: ProtocolVersion) -> bool {
    protocol >= ProtocolVersion::V8
}

async fn handle_session_command(
    stream: &mut dyn StationIo,
    state: &mut SessionState,
    command: SessionCommand,
    context: &SessionContext,
) -> Result<bool, ServerError> {
    let config = &context.config;
    let protocol = state.registration.protocol;
    match command {
        SessionCommand::Confirmed { .. } => {
            unreachable!("confirmed commands are unwrapped by the session loop")
        }
        SessionCommand::OfferIncoming {
            line_instance,
            call_id,
            info,
            presentation,
            ringer,
            delivery: _,
        } => {
            let line_instance = normalize_line(state, line_instance.get());
            let statistics_directory_number = statistics_directory_for_call_info(&info).to_owned();
            let caller = match (
                info.calling_name.trim().is_empty(),
                info.calling_number.trim().is_empty(),
            ) {
                (false, false) => format!("{} ({})", info.calling_name, info.calling_number),
                (false, true) => info.calling_name.clone(),
                (true, false) => info.calling_number.clone(),
                (true, true) => "Unknown number".to_owned(),
            };
            let incoming_state = presentation.call_state();
            let call = insert_call(state, call_id, line_instance, Codec::Pcmu, incoming_state);
            if incoming_state == CallState::RingIn && state.active_call_id.is_none() {
                state.active_call_id = Some(call.call_id);
            }
            if let Some(stored) = state.calls_by_id.get_mut(&call.call_id) {
                stored.statistics_directory_number = statistics_directory_number;
                stored.ringer = ringer;
            }
            send_message(
                stream,
                &ServerMessage::ClearPrompt {
                    line_instance,
                    call_reference: call.wire_reference,
                },
                protocol,
            )
            .await?;
            send_message(
                stream,
                &ServerMessage::CallState {
                    state: incoming_state,
                    line_instance,
                    call_reference: call.wire_reference,
                },
                protocol,
            )
            .await?;
            send_station_ui_message(
                stream,
                state,
                &ServerMessage::CallInfo {
                    info: *info,
                    line_instance,
                    call_reference: call.wire_reference,
                },
            )
            .await?;
            send_message(
                stream,
                &ServerMessage::SetLamp {
                    stimulus: ButtonType::Line,
                    instance: line_instance,
                    mode: LampMode::Blink,
                },
                protocol,
            )
            .await?;
            if let Some(ringer) = incoming_ringer(ringer, incoming_state) {
                let audible = ringer_is_audible(ringer);
                if audible || state.ringer_owner.is_none() {
                    send_message(
                        stream,
                        &ServerMessage::SetRinger {
                            mode: ringer.mode,
                            duration: ringer.duration,
                            line_instance,
                            call_reference: call.wire_reference,
                        },
                        protocol,
                    )
                    .await?;
                }
                if audible {
                    state.ringer_owner = Some(call.call_id);
                }
            }
            state.active_key_mode = KeyMode::RingIn;
            send_message(
                stream,
                &ServerMessage::SelectSoftKeys {
                    line_instance,
                    call_reference: call.wire_reference,
                    set: KeyMode::RingIn,
                    valid_mask: state.device.soft_keys.valid_mask(KeyMode::RingIn),
                },
                protocol,
            )
            .await?;
            send_station_ui_message(
                stream,
                state,
                &ServerMessage::DisplayPrompt {
                    timeout_seconds: 0,
                    text: format!("From {caller}"),
                    line_instance,
                    call_reference: call.wire_reference,
                },
            )
            .await?;
        }
        SessionCommand::Public(command) => {
            let command = *command;
            if let Some(call_id) = command_call_id(&command)
                && !matches!(
                    &command.action,
                    CommandAction::BeginCall { .. } | CommandAction::CloseCall { .. }
                )
                && !state.calls_by_id.contains_key(&call_id)
            {
                debug!(device_id = %state.device.id, ?call_id, command = ?command, "ignoring stale SCCP call command");
                return Ok(false);
            }
            let action = command.action;
            match action {
                CommandAction::DisconnectDevice { .. } => {
                    return Ok(true);
                }
                CommandAction::BeginCall {
                    line_instance,
                    call_id,
                    codec,
                } => {
                    if state.calls_by_id.contains_key(&call_id) {
                        return Ok(false);
                    }
                    let line_instance = normalize_line(state, line_instance.get());
                    let call =
                        insert_call(state, call_id, line_instance, codec, CallState::OffHook);
                    state.active_call_id = Some(call.call_id);
                    state.active_key_mode = KeyMode::OffHook;
                    begin_phone_call_ui(stream, &call, &state.device, state.station_context())
                        .await?;
                }
                CommandAction::BeginTransfer {
                    source_call_id,
                    consultation_line_instance,
                    consultation_call_id,
                    codec,
                } => {
                    let consultation_line_instance = consultation_line_instance.get();
                    if state.calls_by_id.contains_key(&consultation_call_id) {
                        return Ok(false);
                    }
                    let source = require_call_mut(state, source_call_id)?;
                    if !matches!(
                        source.state,
                        CallState::Hold | CallState::HoldYellow | CallState::HoldRed
                    ) {
                        return Err(ServerError::InvalidCallTransaction {
                            call_id: source_call_id,
                            operation: "begin transfer",
                            state: source.state,
                        });
                    }
                    source.state = CallState::Transfer;
                    source.transfer_role = Some(SessionTransferRole::Source {
                        consultation_call_id,
                    });
                    let source = source.clone();
                    send_message(
                        stream,
                        &ServerMessage::CallState {
                            state: CallState::Transfer,
                            line_instance: source.line_instance,
                            call_reference: source.wire_reference,
                        },
                        protocol,
                    )
                    .await?;
                    send_station_ui_message(
                        stream,
                        state,
                        &ServerMessage::DisplayPrompt {
                            timeout_seconds: 0,
                            text: "Call Transfer".into(),
                            line_instance: source.line_instance,
                            call_reference: source.wire_reference,
                        },
                    )
                    .await?;

                    let line_instance = normalize_line(state, consultation_line_instance);
                    let mut consultation = insert_call(
                        state,
                        consultation_call_id,
                        line_instance,
                        codec,
                        CallState::OffHook,
                    );
                    consultation.transfer_role =
                        Some(SessionTransferRole::Consultation { source_call_id });
                    state
                        .calls_by_id
                        .insert(consultation_call_id, consultation.clone());
                    state.active_call_id = Some(consultation.call_id);
                    state.active_key_mode = KeyMode::OffHookFeature;
                    begin_phone_call_ui_with_key_mode(
                        stream,
                        &consultation,
                        &state.device,
                        KeyMode::OffHookFeature,
                        state.station_context(),
                    )
                    .await?;
                    send_message(
                        stream,
                        &ServerMessage::SetLamp {
                            stimulus: ButtonType::Transfer,
                            instance: source.line_instance,
                            mode: LampMode::Flash,
                        },
                        protocol,
                    )
                    .await?;
                }
                CommandAction::SetCallSelected {
                    call_id, selected, ..
                } => {
                    let call = require_call(state, call_id)?.clone();
                    send_message(
                        stream,
                        &ServerMessage::CallSelectStatus {
                            status: u32::from(selected),
                            call_reference: call.wire_reference,
                            line_instance: call.line_instance,
                        },
                        protocol,
                    )
                    .await?;
                }
                CommandAction::SetMwi {
                    line_instance,
                    enabled,
                    ..
                } => {
                    let line_instance = line_instance.get();
                    state.mwi_by_line.insert(line_instance, enabled);
                    send_mwi_lamp(stream, state, line_instance, enabled, protocol).await?;
                }
                CommandAction::SetForwardStatus {
                    line_instance,
                    forward_all,
                    forward_busy,
                    forward_no_answer,
                    ..
                } => {
                    let line_instance = line_instance.get();
                    state.forwarding_by_line.insert(
                        line_instance,
                        SessionForwarding {
                            all: forward_all.clone(),
                            busy: forward_busy.clone(),
                            no_answer: forward_no_answer.clone(),
                        },
                    );
                    send_message(
                        stream,
                        &ServerMessage::ForwardStatus {
                            line_instance,
                            forward_all,
                            forward_busy,
                            forward_no_answer,
                        },
                        protocol,
                    )
                    .await?;
                }
                CommandAction::SetFeatureStatus {
                    instance, enabled, ..
                } => {
                    let instance = instance.get();
                    if let Some(messages) = feature_state_messages(&state.device, instance, enabled)
                    {
                        cache_feature_projection(&mut state.feature_states, instance, &messages[0]);
                        for message in messages {
                            send_station_ui_message(stream, state, &message).await?;
                        }
                    }
                }
                CommandAction::SetDoNotDisturbStatus {
                    instance,
                    mode,
                    button_mode,
                    ..
                } => {
                    let instance = instance.get();
                    if let Some(messages) = do_not_disturb_state_messages(
                        &state.device,
                        instance,
                        mode,
                        button_mode,
                        protocol,
                    ) {
                        cache_feature_projection(&mut state.feature_states, instance, &messages[0]);
                        for message in messages {
                            send_station_ui_message(stream, state, &message).await?;
                        }
                    }
                }
                CommandAction::SetRecordingButtonStatus {
                    state: recording_state,
                } => {
                    let instances = state
                        .device
                        .buttons
                        .iter()
                        .filter_map(|button| match button {
                            ButtonDefinition::Recording(recording) => Some(recording.instance),
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    for instance in instances {
                        let Some(messages) = recording_button_state_messages(
                            &state.device,
                            instance,
                            recording_state,
                            protocol,
                            state.registration.device_type,
                            state.features,
                        ) else {
                            continue;
                        };
                        cache_feature_projection(&mut state.feature_states, instance, &messages[0]);
                        for message in messages {
                            send_station_ui_message(stream, state, &message).await?;
                        }
                    }
                }
                CommandAction::SetMobilityAppearance {
                    mobility_instance,
                    appearance,
                    ..
                } => {
                    let mobility_instance = mobility_instance.get();
                    let configured = state.device.buttons.iter().any(|button| {
                        matches!(
                            button,
                            ButtonDefinition::Feature(feature)
                                if feature.instance == mobility_instance
                                    && feature.feature == ButtonType::Mobility
                        )
                    });
                    if !configured {
                        return Err(CodecError::InvalidDefinition(format!(
                            "device {} has no mobility button instance {mobility_instance}",
                            state.device.id
                        ))
                        .into());
                    }
                    let previous = state.mobility_appearances.get(&mobility_instance).cloned();
                    let mut next_appearances = state.mobility_appearances.clone();
                    match &appearance {
                        Some(appearance) => {
                            next_appearances.insert(mobility_instance, appearance.clone());
                        }
                        None => {
                            next_appearances.remove(&mobility_instance);
                        }
                    }
                    let candidate = mobility_device_candidate(
                        &state.device,
                        &state.mobility_appearances,
                        &next_appearances,
                    )?;

                    send_button_template(
                        stream,
                        &candidate,
                        protocol,
                        state.registration.device_type,
                    )
                    .await?;
                    if let Some(appearance) = &appearance {
                        if let Some(message) = line_status(&candidate, appearance.instance) {
                            send_station_ui_message(stream, state, &message).await?;
                        }
                    } else if let Some(previous) = &previous {
                        send_station_ui_message(
                            stream,
                            state,
                            &ServerMessage::LineStatus {
                                instance: previous.instance,
                                directory_number: String::new(),
                                fully_qualified_display_name: String::new(),
                                display_label: String::new(),
                            },
                        )
                        .await?;
                    }
                    state.device = candidate;
                    state.mobility_appearances = next_appearances;
                }
                CommandAction::SetBlfStatus {
                    instance,
                    state: blf_state,
                    caller,
                    ..
                } => {
                    let instance = instance.get();
                    let Some(message) = blf_status_message(&state.device, instance, blf_state)
                    else {
                        return Err(ServerError::UnknownBlfButton {
                            device: state.device.id.clone(),
                            instance,
                        });
                    };
                    let ServerMessage::FeatureStatus { ref label, .. } = message else {
                        unreachable!("BLF status is a feature-state message")
                    };
                    cache_feature_projection(&mut state.feature_states, instance, &message);
                    send_station_ui_message(stream, state, &message).await?;
                    let notification = hinted_ringing_notification(
                        &state.device,
                        label,
                        caller.as_ref(),
                        blf_state,
                    );
                    if let Some(notification) = reconcile_blf_alert(
                        instance,
                        notification,
                        &mut state.runtime.active_blf_alerts,
                        &mut state.runtime.visible_blf_alert,
                    ) {
                        for message in status_message_frames(
                            notification,
                            state.registration.device_type,
                            &mut state.persistent_status_message,
                        ) {
                            send_station_ui_message(stream, state, &message).await?;
                        }
                    }
                }
                CommandAction::ShowParkingMenu {
                    instance,
                    transaction_id,
                    lot,
                    calls,
                    ..
                } => {
                    let instance = instance.get();
                    let transaction_id = transaction_id.get();
                    send_message(
                        stream,
                        &ServerMessage::UserToDeviceDataV1(UserDataV1Message {
                            application_id: PARKING_APPLICATION_ID,
                            line_instance: instance,
                            call_reference: 0,
                            transaction_id,
                            sequence_flag: 0,
                            display_priority: 2,
                            conference_id: 0,
                            application_instance_id: instance,
                            routing: 0,
                            data: parking_menu_xml(instance, transaction_id, &lot, &calls)?
                                .into_bytes(),
                        }),
                        protocol,
                    )
                    .await?;
                    state.pending_parking_menu = Some(PendingParkingMenu {
                        instance,
                        transaction_id,
                    });
                }
                CommandAction::ShowConferenceList {
                    call_id,
                    conference_id,
                    participants,
                    ..
                } => {
                    let call = require_call(state, call_id)?.clone();
                    let family = if protocol >= ProtocolVersion::V8 {
                        ConferenceMenuFamily::IconMenu
                    } else {
                        ConferenceMenuFamily::Menu
                    };
                    let data = ConferenceListDocument::new(conference_id, &participants, family)?
                        .to_xml()?
                        .into_bytes();
                    send_message(
                        stream,
                        &ServerMessage::UserToDeviceDataV1(UserDataV1Message {
                            application_id: ConferenceListAction::APPLICATION_ID,
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                            transaction_id: conference_id.get(),
                            sequence_flag: 0,
                            display_priority: 2,
                            conference_id: conference_id.get(),
                            application_instance_id: call.line_instance,
                            routing: 0,
                            data,
                        }),
                        protocol,
                    )
                    .await?;
                }
                CommandAction::ShowConferenceParticipantActions {
                    call_id,
                    conference_id,
                    participant,
                    removable,
                    demotable,
                    ..
                } => {
                    let call = require_call(state, call_id)?.clone();
                    let family = if protocol >= ProtocolVersion::V8 {
                        ConferenceMenuFamily::IconMenu
                    } else {
                        ConferenceMenuFamily::Menu
                    };
                    let data = ConferenceParticipantActionsDocument::new(
                        conference_id,
                        &participant,
                        removable,
                        demotable,
                        family,
                    )?
                    .to_xml()?
                    .into_bytes();
                    send_message(
                        stream,
                        &ServerMessage::UserToDeviceDataV1(UserDataV1Message {
                            application_id: ConferenceListAction::APPLICATION_ID,
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                            transaction_id: conference_id.get(),
                            sequence_flag: 0,
                            display_priority: 2,
                            conference_id: conference_id.get(),
                            application_instance_id: call.line_instance,
                            routing: 0,
                            data,
                        }),
                        protocol,
                    )
                    .await?;
                }
                CommandAction::ShowTextService {
                    line_instance,
                    call_reference,
                    transaction_id,
                    priority,
                    document,
                    ..
                } => {
                    for message in text_service_messages(
                        line_instance,
                        call_reference,
                        transaction_id,
                        priority,
                        &document,
                        protocol,
                    )? {
                        send_message(stream, &message, protocol).await?;
                    }
                }
                CommandAction::ShowInputService {
                    line_instance,
                    call_reference,
                    application_id,
                    transaction_id,
                    priority,
                    document,
                    ..
                } => {
                    for message in input_service_messages(
                        line_instance,
                        call_reference,
                        application_id,
                        transaction_id,
                        priority,
                        &document,
                        protocol,
                    )? {
                        send_message(stream, &message, protocol).await?;
                    }
                }
                CommandAction::ExecutePhoneActions {
                    line_instance,
                    call_reference,
                    application_id,
                    transaction_id,
                    priority,
                    document,
                    ..
                } => {
                    for message in execute_phone_action_messages(
                        line_instance,
                        call_reference,
                        application_id,
                        transaction_id,
                        priority,
                        &document,
                        protocol,
                    )? {
                        send_message(stream, &message, protocol).await?;
                    }
                }
                CommandAction::ShowImageService {
                    line_instance,
                    call_reference,
                    application_id,
                    transaction_id,
                    priority,
                    document,
                    ..
                } => {
                    for message in image_service_messages(
                        line_instance,
                        call_reference,
                        application_id,
                        transaction_id,
                        priority,
                        &document,
                        protocol,
                    )? {
                        send_message(stream, &message, protocol).await?;
                    }
                }
                CommandAction::ShowStatusService {
                    line_instance,
                    call_reference,
                    application_id,
                    transaction_id,
                    priority,
                    document,
                    ..
                } => {
                    for message in status_service_messages(
                        line_instance,
                        call_reference,
                        application_id,
                        transaction_id,
                        priority,
                        &document,
                        protocol,
                    )? {
                        send_message(stream, &message, protocol).await?;
                    }
                }
                CommandAction::SetBackgroundImage {
                    transaction_id,
                    document,
                    ..
                } => {
                    let message = background_control_message(
                        transaction_id,
                        &PhoneBackgroundControlDocument::Set(document),
                    )?;
                    send_message(stream, &message, protocol).await?;
                }
                CommandAction::PreviewBackgroundImage {
                    transaction_id,
                    document,
                    ..
                } => {
                    let message = background_control_message(
                        transaction_id,
                        &PhoneBackgroundControlDocument::Preview(document),
                    )?;
                    send_message(stream, &message, protocol).await?;
                }
                CommandAction::SetRingtone {
                    transaction_id,
                    document,
                    ..
                } => {
                    let message = ringtone_control_message(transaction_id, &document)?;
                    send_message(stream, &message, protocol).await?;
                }
                CommandAction::StartTone { call_id, tone, .. } => {
                    let call = require_call(state, call_id)?.clone();
                    let message = if tone == Tone::Silence {
                        ServerMessage::StopTone {
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        }
                    } else {
                        ServerMessage::StartTone {
                            tone,
                            direction: ToneDirection::User,
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        }
                    };
                    send_message(stream, &message, protocol).await?;
                }
                CommandAction::StartAnnouncement {
                    conference_id,
                    announcements,
                    end_of_ack,
                    participant_ids,
                    hearing_participant_mask,
                    play_mode,
                    ..
                } => {
                    let _ = (
                        conference_id,
                        announcements,
                        end_of_ack,
                        participant_ids,
                        hearing_participant_mask,
                        play_mode,
                    );
                    return Err(ServerError::InvalidStationCommand {
                        message: "StartAnnouncement",
                    });
                }
                CommandAction::StopAnnouncement { conference_id, .. } => {
                    let _ = conference_id;
                    return Err(ServerError::InvalidStationCommand {
                        message: "StopAnnouncement",
                    });
                }
                CommandAction::AnnouncementFinish {
                    conference_id,
                    play_status,
                    ..
                } => {
                    let _ = (conference_id, play_status);
                    return Err(ServerError::InvalidStationCommand {
                        message: "AnnouncementFinish",
                    });
                }
                CommandAction::SetCallInfo { call_id, info, .. } => {
                    let statistics_directory_number =
                        statistics_directory_for_call_info(&info).to_owned();
                    if let Some(stored) = state.calls_by_id.get_mut(&call_id) {
                        stored.statistics_directory_number = statistics_directory_number;
                    }
                    let call = require_call(state, call_id)?.clone();
                    send_station_ui_message(
                        stream,
                        state,
                        &ServerMessage::CallInfo {
                            info,
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                    )
                    .await?;
                }
                CommandAction::CommitOutboundCall { call_id, info, .. } => {
                    let statistics_directory_number =
                        statistics_directory_for_call_info(&info).to_owned();
                    let call = require_call_mut(state, call_id)?;
                    call.state = CallState::Proceed;
                    call.history_disposition =
                        updated_history_disposition(call.history_disposition, CallState::Proceed);
                    call.statistics_directory_number = statistics_directory_number;
                    let call = call.clone();
                    let number = digit_character(config.dial_terminator)
                        .and_then(|terminator| call.dialed_number.strip_suffix(terminator))
                        .unwrap_or(&call.dialed_number)
                        .to_owned();
                    remember_last_number(state, call.line_instance, &number, config);
                    state.active_call_id = Some(call.call_id);
                    refresh_mwi_lamps(stream, state, protocol).await?;
                    for message in [
                        ServerMessage::StopTone {
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                        ServerMessage::SetLamp {
                            stimulus: ButtonType::Line,
                            instance: call.line_instance,
                            mode: LampMode::Blink,
                        },
                        ServerMessage::CallInfo {
                            info,
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                        ServerMessage::DialedNumber {
                            number,
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                        ServerMessage::CallState {
                            state: CallState::Proceed,
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                    ] {
                        send_station_ui_message(stream, state, &message).await?;
                    }
                }
                CommandAction::PresentOutboundProceeding { call_id, info, .. } => {
                    let statistics_directory_number =
                        statistics_directory_for_call_info(&info).to_owned();
                    let call = require_call_mut(state, call_id)?;
                    call.state = CallState::Proceed;
                    call.history_disposition =
                        updated_history_disposition(call.history_disposition, CallState::Proceed);
                    call.statistics_directory_number = statistics_directory_number;
                    let call = call.clone();
                    state.active_call_id = Some(call.call_id);
                    refresh_mwi_lamps(stream, state, protocol).await?;
                    for message in [
                        ServerMessage::StopTone {
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                        ServerMessage::CallState {
                            state: CallState::Proceed,
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                        ServerMessage::CallInfo {
                            info,
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                        ServerMessage::DisplayPrompt {
                            timeout_seconds: 0,
                            text: "Call Proceed".into(),
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                    ] {
                        send_station_ui_message(stream, state, &message).await?;
                    }
                }
                CommandAction::PresentOutboundRinging { call_id, info, .. } => {
                    let statistics_directory_number =
                        statistics_directory_for_call_info(&info).to_owned();
                    let call = require_call_mut(state, call_id)?;
                    call.state = CallState::Proceed;
                    call.history_disposition =
                        updated_history_disposition(call.history_disposition, CallState::Proceed);
                    call.statistics_directory_number = statistics_directory_number;
                    let call = call.clone();
                    state.active_call_id = Some(call.call_id);
                    let key_mode = transfer_key_mode(&call, CallState::RingOut);
                    state.active_key_mode = key_mode;
                    refresh_mwi_lamps(stream, state, protocol).await?;
                    for message in [
                        ServerMessage::CallState {
                            state: CallState::Proceed,
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                        ServerMessage::DisplayPrompt {
                            timeout_seconds: 0,
                            text: "Ring out".into(),
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                        ServerMessage::StopTone {
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                        ServerMessage::StartTone {
                            tone: Tone::Alerting,
                            direction: ToneDirection::User,
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                        ServerMessage::SelectSoftKeys {
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                            set: key_mode,
                            valid_mask: state.device.soft_keys.valid_mask(key_mode),
                        },
                        ServerMessage::CallInfo {
                            info,
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                    ] {
                        send_station_ui_message(stream, state, &message).await?;
                    }
                }
                CommandAction::SetCallState {
                    call_id,
                    state: call_state,
                    ..
                } => {
                    let transfer_source_to_clear =
                        state
                            .calls_by_id
                            .get(&call_id)
                            .and_then(|call| match call.transfer_role {
                                Some(SessionTransferRole::Source {
                                    consultation_call_id,
                                }) if call_state != CallState::Transfer => {
                                    Some((consultation_call_id, call.line_instance))
                                }
                                _ => None,
                            });
                    let call = require_call_mut(state, call_id)?;
                    call.state = call_state;
                    call.history_disposition =
                        updated_history_disposition(call.history_disposition, call_state);
                    let call = call.clone();
                    if matches!(
                        call_state,
                        CallState::Proceed | CallState::RingOut | CallState::Connected
                    ) {
                        remember_last_number(
                            state,
                            call.line_instance,
                            &call.dialed_number,
                            config,
                        );
                    }
                    prepare_call_state_ui(stream, &call, call_state, protocol).await?;
                    send_message(
                        stream,
                        &ServerMessage::CallState {
                            state: call_state,
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                        protocol,
                    )
                    .await?;
                    finish_call_state_ui(stream, &call, call_state, state.station_context())
                        .await?;
                    let set = transfer_key_mode(&call, call_state);
                    state.active_key_mode = set;
                    match call_state {
                        CallState::Connected
                        | CallState::OffHook
                        | CallState::Transfer
                        | CallState::RingOut
                        | CallState::Proceed
                        | CallState::IntercomOneWay => {
                            state.active_call_id = Some(call.call_id);
                        }
                        CallState::OnHook
                        | CallState::Hold
                        | CallState::HoldYellow
                        | CallState::HoldRed
                            if state.active_call_id == Some(call.call_id) =>
                        {
                            state.active_call_id = None;
                        }
                        _ => {}
                    }
                    refresh_mwi_lamps(stream, state, protocol).await?;
                    send_message(
                        stream,
                        &ServerMessage::SelectSoftKeys {
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                            set,
                            valid_mask: state.device.soft_keys.valid_mask(set),
                        },
                        protocol,
                    )
                    .await?;
                    if let Some((consultation_call_id, line_instance)) = transfer_source_to_clear {
                        if let Some(source) = state.calls_by_id.get_mut(&call_id) {
                            source.transfer_role = None;
                        }
                        if let Some(consultation) = state.calls_by_id.get_mut(&consultation_call_id)
                        {
                            consultation.transfer_role = None;
                        }
                        send_message(
                            stream,
                            &ServerMessage::SetLamp {
                                stimulus: ButtonType::Transfer,
                                instance: line_instance,
                                mode: LampMode::Off,
                            },
                            protocol,
                        )
                        .await?;
                    }
                }
                CommandAction::DisplayPrompt {
                    call_id,
                    timeout_seconds,
                    text,
                    ..
                } => {
                    let call = require_call(state, call_id)?.clone();
                    send_station_ui_message(
                        stream,
                        state,
                        &ServerMessage::DisplayPrompt {
                            timeout_seconds,
                            text,
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                    )
                    .await?;
                }
                CommandAction::ClearPrompt { call_id, .. } => {
                    let call = require_call(state, call_id)?.clone();
                    send_message(
                        stream,
                        &ServerMessage::ClearPrompt {
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                        protocol,
                    )
                    .await?;
                }
                CommandAction::SetStatusMessage { message, beep, .. } => {
                    let frames = status_message_frames(
                        message,
                        state.registration.device_type,
                        &mut state.persistent_status_message,
                    );
                    for frame in frames {
                        send_station_ui_message(stream, state, &frame).await?;
                    }
                    if beep {
                        send_message(
                            stream,
                            &ServerMessage::StartTone {
                                tone: Tone::ZipZip,
                                direction: ToneDirection::User,
                                line_instance: 0,
                                call_reference: 0,
                            },
                            protocol,
                        )
                        .await?;
                    }
                }
                CommandAction::SetMicrophoneMode { enabled, .. } => {
                    send_message(
                        stream,
                        &ServerMessage::SetMicrophoneMode(if enabled {
                            MicrophoneMode::On
                        } else {
                            MicrophoneMode::Off
                        }),
                        protocol,
                    )
                    .await?;
                }
                CommandAction::SetRecordingStatus {
                    call_id, active, ..
                } => {
                    let call = require_call(state, call_id)?.clone();
                    send_message(
                        stream,
                        &ServerMessage::RecordingStatus {
                            call_reference: call.wire_reference,
                            active,
                        },
                        protocol,
                    )
                    .await?;
                }
                CommandAction::ResetDevice { reset_type, .. } => {
                    send_message(stream, &ServerMessage::Reset(reset_type), protocol).await?;
                }
                ringing @ (CommandAction::StartRinging { call_id }
                | CommandAction::StopRinging { call_id }) => {
                    let enabled = matches!(ringing, CommandAction::StartRinging { .. });
                    let call = require_call(state, call_id)?.clone();
                    if let Some(stored) = state.calls_by_id.get_mut(&call_id) {
                        stored.ringer = enabled.then_some(IncomingRing::default());
                    }
                    if !enabled && state.ringer_owner != Some(call_id) {
                        return Ok(false);
                    }
                    send_message(
                        stream,
                        &ServerMessage::SetRinger {
                            mode: if enabled {
                                RingerMode::Inside
                            } else {
                                RingerMode::Off
                            },
                            duration: RingDuration::Normal,
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                        protocol,
                    )
                    .await?;
                    state.ringer_owner = enabled.then_some(call_id);
                    if !enabled {
                        let order = *context
                            .call_answer_order
                            .read()
                            .expect("SCCP call-answer-order lock poisoned");
                        if let Some((call_id, promote)) = incoming_successor(state, call_id, order)
                        {
                            present_incoming_successor(stream, state, call_id, promote).await?;
                        }
                    }
                }
                CommandAction::OpenReceiveChannel {
                    call_id,
                    purpose,
                    source,
                    codec,
                    packet_ms,
                    max_frames_per_packet,
                    dtmf_mode,
                    audio_processing,
                    ..
                } => {
                    if purpose == ReceiveChannelPurpose::InboundAnswer {
                        let call_state = require_call(state, call_id)?.state;
                        if call_state != CallState::OffHook {
                            return Err(ServerError::InvalidCallTransaction {
                                call_id,
                                operation: "open inbound answer media",
                                state: call_state,
                            });
                        }
                    }
                    let telephone_event_payload = dtmf_mode.telephone_event_payload(state.features);
                    let request = allocate_media_request_identity(state, call_id)?;
                    let call = require_call_mut(state, call_id)?;
                    call.media.requested = true;
                    call.media.codec = codec;
                    call.media.packet_ms = packet_ms;
                    call.media.max_frames_per_packet = max_frames_per_packet;
                    call.media.receive.telephone_event_payload = telephone_event_payload;
                    call.media.receive.peer = None;
                    call.media.receive.state = MediaChannelState::Opening;
                    call.media.receive.deadline = None;
                    call.media.receive.request = Some(request);
                    if call.media.transmit.state == MediaChannelState::Closed {
                        call.media.transmit.request = None;
                    }
                    call.media.coupled_transmit_endpoint = None;
                    let call = call.clone();
                    if purpose == ReceiveChannelPurpose::InboundAnswer {
                        send_message(
                            stream,
                            &ServerMessage::CallState {
                                state: CallState::Connected,
                                line_instance: call.line_instance,
                                call_reference: call.wire_reference,
                            },
                            protocol,
                        )
                        .await?;
                    }
                    send_message(
                        stream,
                        &ServerMessage::OpenReceiveChannel {
                            call_reference: call.wire_reference,
                            passthrough_party_id: request.token().get(),
                            packet_ms,
                            codec,
                            echo_cancellation: audio_processing.echo_cancellation,
                            telephone_event_payload,
                            source_address: source
                                .map(|endpoint| endpoint.address)
                                .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
                            source_port: source.map_or(0, |endpoint| endpoint.rtp_port),
                            encryption: None,
                            wire: None,
                        },
                        protocol,
                    )
                    .await?;
                    require_call_mut(state, call_id)?.media.receive.deadline =
                        Some(Instant::now() + HANDSET_ACKNOWLEDGEMENT_TIMEOUT);
                }
                CommandAction::OpenMultimediaReceiveChannel {
                    call_id,
                    descriptor,
                } => {
                    let call_state = require_call(state, call_id)?.state;
                    if call_state != CallState::Connected {
                        return Err(ServerError::InvalidCallTransaction {
                            call_id,
                            operation: "open video receive media",
                            state: call_state,
                        });
                    }
                    validate_multimedia_receive(state, &descriptor)?;
                    let request = allocate_video_receive_identity(state, call_id)?;
                    let replacement_close = take_multimedia_receive_close(state, call_id);
                    let call = require_call_mut(state, call_id)?;
                    let line_instance = call.line_instance;
                    let call_reference = CallReference::new(call.wire_reference);
                    call.video_receive.leg = Some(VideoReceiveLeg {
                        request,
                        conference_id: descriptor.conference_id,
                        codec: descriptor.payload.codec(),
                        requested_address_type: descriptor.requested_address_type,
                        state: MediaChannelState::Opening,
                        deadline: Some(Instant::now() + HANDSET_ACKNOWLEDGEMENT_TIMEOUT),
                    });

                    if let Some(close) = replacement_close {
                        send_message(stream, &close, protocol).await?;
                    }
                    send_message(
                        stream,
                        &ServerMessage::OpenMultimediaChannel(OpenMultimediaChannel {
                            conference_id: descriptor.conference_id,
                            passthrough_party_id: request.token().get().into(),
                            line_instance,
                            call_reference,
                            payload: descriptor.payload,
                            conference_creator: descriptor.conference_creator,
                            encryption: descriptor.encryption,
                            stream_passthrough_id: descriptor.stream_passthrough_id,
                            associated_stream_id: descriptor.associated_stream_id,
                            source: descriptor.source,
                            requested_address_type: descriptor.requested_address_type,
                        }),
                        protocol,
                    )
                    .await?;
                }
                CommandAction::CloseMultimediaReceiveChannel { call_id } => {
                    if let Some(close) = take_multimedia_receive_close(state, call_id) {
                        send_message(stream, &close, protocol).await?;
                    }
                }
                CommandAction::StartMultimediaTransmission {
                    call_id,
                    descriptor,
                } => {
                    let call_state = require_call(state, call_id)?.state;
                    if call_state != CallState::Connected {
                        return Err(ServerError::InvalidCallTransaction {
                            call_id,
                            operation: "start video transmit media",
                            state: call_state,
                        });
                    }
                    validate_multimedia_transmit(state, &descriptor)?;
                    let request = allocate_video_transmit_identity(state, call_id)?;
                    let replacement_stop = take_multimedia_transmit_stop(state, call_id);
                    let call_reference = {
                        let call = require_call_mut(state, call_id)?;
                        let call_reference = CallReference::new(call.wire_reference);
                        call.video_transmit.leg = Some(VideoTransmitLeg {
                            request,
                            conference_id: descriptor.conference_id,
                            codec: descriptor.payload.codec(),
                            address_type: address_type(descriptor.endpoint.address),
                            state: MediaChannelState::Opening,
                            deadline: Some(Instant::now() + HANDSET_ACKNOWLEDGEMENT_TIMEOUT),
                        });
                        call_reference
                    };

                    if let Some(stop) = replacement_stop {
                        send_message(stream, &stop, protocol).await?;
                    }
                    send_message(
                        stream,
                        &ServerMessage::StartMultimediaTransmission(MultimediaTransmissionStart {
                            conference_id: descriptor.conference_id,
                            passthrough_party_id: request.token().get().into(),
                            endpoint: descriptor.endpoint,
                            call_reference,
                            payload: descriptor.payload,
                            traffic_class: descriptor.traffic_class,
                            encryption: descriptor.encryption,
                            stream_passthrough_id: descriptor.stream_passthrough_id,
                            associated_stream_id: descriptor.associated_stream_id,
                        }),
                        protocol,
                    )
                    .await?;
                }
                CommandAction::StopMultimediaTransmission { call_id } => {
                    if let Some(stop) = take_multimedia_transmit_stop(state, call_id) {
                        send_message(stream, &stop, protocol).await?;
                    }
                }
                flow_action @ (CommandAction::SetMultimediaTransmitBitRate {
                    call_id,
                    passthrough_party_id,
                    maximum_bit_rate,
                }
                | CommandAction::NotifyMultimediaTransmitBitRate {
                    call_id,
                    passthrough_party_id,
                    maximum_bit_rate,
                }) => {
                    if maximum_bit_rate == 0 {
                        return Err(ServerError::InvalidMultimediaTransmitControl(
                            "maximum bit rate must be nonzero",
                        ));
                    }
                    let (conference_id, call_reference) =
                        multimedia_transmit_control_identity(state, call_id, passthrough_party_id)?;
                    let flow = VideoFlowControl {
                        conference_id,
                        passthrough_party_id,
                        call_reference,
                        maximum_bit_rate,
                    };
                    let message = if matches!(
                        flow_action,
                        CommandAction::SetMultimediaTransmitBitRate { .. }
                    ) {
                        ServerMessage::FlowControlCommand(flow)
                    } else {
                        ServerMessage::FlowControlNotify(flow)
                    };
                    send_message(stream, &message, protocol).await?;
                }
                CommandAction::ControlMultimediaTransmission {
                    call_id,
                    passthrough_party_id,
                    control,
                } => {
                    let (conference_id, call_reference) =
                        multimedia_transmit_control_identity(state, call_id, passthrough_party_id)?;
                    let (command, data) = encode_multimedia_transmit_control(control)?;
                    send_message(
                        stream,
                        &ServerMessage::MiscellaneousCommand(MiscellaneousCommand {
                            conference_id,
                            passthrough_party_id,
                            call_reference,
                            command,
                            data,
                        }),
                        protocol,
                    )
                    .await?;
                }
                CommandAction::OpenOutboundMedia {
                    call_id,
                    source,
                    mut endpoint,
                    codec,
                    packet_ms,
                    max_frames_per_packet,
                    dtmf_mode,
                    audio_processing,
                    traffic_class,
                } => {
                    let call_state = require_call(state, call_id)?.state;
                    if !matches!(call_state, CallState::Proceed | CallState::RingOut) {
                        return Err(ServerError::InvalidCallTransaction {
                            call_id,
                            operation: "open coupled outbound media",
                            state: call_state,
                        });
                    }
                    let telephone_event_payload = dtmf_mode.telephone_event_payload(state.features);
                    let source_address = source
                        .map(|source| source.address)
                        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
                    let source_port = source.map_or(0, |source| source.rtp_port);
                    let request = allocate_media_request_identity(state, call_id)?;
                    let call = require_call_mut(state, call_id)?;
                    call.media.requested = true;
                    call.media.codec = codec;
                    call.media.packet_ms = packet_ms;
                    call.media.max_frames_per_packet = max_frames_per_packet;
                    call.media.receive.telephone_event_payload = telephone_event_payload;
                    call.media.receive.peer = None;
                    call.media.receive.state = MediaChannelState::Opening;
                    call.media.receive.deadline = None;
                    call.media.receive.request = Some(request);
                    endpoint.telephone_event_payload = telephone_event_payload;
                    call.media.transmit.telephone_event_payload = telephone_event_payload;
                    call.media.transmit.peer = Some(endpoint);
                    call.media.transmit.state = MediaChannelState::Open;
                    call.media.transmit.deadline = None;
                    call.media.transmit.request = Some(request);
                    call.media.transmit_confirmation = TransmitConfirmation::Inactive;
                    call.media.coupled_transmit_endpoint = Some(endpoint);
                    let call = call.clone();
                    send_message(
                        stream,
                        &ServerMessage::OpenReceiveChannel {
                            call_reference: call.wire_reference,
                            passthrough_party_id: request.token().get(),
                            packet_ms,
                            codec,
                            echo_cancellation: audio_processing.echo_cancellation,
                            telephone_event_payload,
                            source_address,
                            source_port,
                            encryption: None,
                            wire: None,
                        },
                        protocol,
                    )
                    .await?;
                    send_message(
                        stream,
                        &ServerMessage::StartMediaTransmission {
                            call_reference: call.wire_reference,
                            passthrough_party_id: request.token().get(),
                            endpoint,
                            silence_suppression: audio_processing.silence_suppression,
                            traffic_class,
                            encryption: None,
                            wire: None,
                        },
                        protocol,
                    )
                    .await?;
                    let deadline = Instant::now() + HANDSET_ACKNOWLEDGEMENT_TIMEOUT;
                    let call = require_call_mut(state, call_id)?;
                    call.media.receive.deadline = Some(deadline);
                    call.media.transmit_confirmation = TransmitConfirmation::Awaiting { deadline };
                }
                CommandAction::CloseReceiveChannel { call_id, .. } => {
                    let call = require_call_mut(state, call_id)?;
                    call.media.coupled_transmit_endpoint = None;
                    if call.media.receive.state != MediaChannelState::Closed {
                        call.media.receive.state = MediaChannelState::Closed;
                        call.media.receive.deadline = None;
                        let call = call.clone();
                        send_message(
                            stream,
                            &ServerMessage::CloseReceiveChannel(AudioStreamControl {
                                conference_id: ConferenceId::new(call.wire_reference),
                                call_reference: CallReference::new(call.wire_reference),
                                passthrough_party_id: media_request_party_id(
                                    call.media.receive.request,
                                    call.wire_reference,
                                )
                                .into(),
                                port_handling_flag: 0,
                            }),
                            protocol,
                        )
                        .await?;
                    }
                }
                CommandAction::StartMedia {
                    call_id,
                    mut endpoint,
                    dtmf_mode,
                    audio_processing,
                    traffic_class,
                } => {
                    let telephone_event_payload = dtmf_mode.telephone_event_payload(state.features);
                    let request = {
                        let call = require_call(state, call_id)?;
                        if call.media.transmit.request.is_none() {
                            call.media.receive.request
                        } else {
                            None
                        }
                    };
                    let request = match request {
                        Some(request) => request,
                        None => allocate_media_request_identity(state, call_id)?,
                    };
                    let call = require_call_mut(state, call_id)?;
                    call.media.requested = true;
                    call.media.transmit.telephone_event_payload = telephone_event_payload;
                    endpoint.telephone_event_payload = telephone_event_payload;
                    call.media.transmit.peer = Some(endpoint);
                    call.media.transmit.state = MediaChannelState::Open;
                    call.media.transmit.deadline = None;
                    call.media.transmit.request = Some(request);
                    call.media.transmit_confirmation = TransmitConfirmation::Awaiting {
                        deadline: Instant::now() + HANDSET_ACKNOWLEDGEMENT_TIMEOUT,
                    };
                    call.media.coupled_transmit_endpoint = None;
                    let call = call.clone();
                    send_message(
                        stream,
                        &ServerMessage::StartMediaTransmission {
                            call_reference: call.wire_reference,
                            passthrough_party_id: request.token().get(),
                            endpoint,
                            silence_suppression: audio_processing.silence_suppression,
                            traffic_class,
                            encryption: None,
                            wire: None,
                        },
                        protocol,
                    )
                    .await?;
                }
                CommandAction::StartMulticastReception {
                    conference_id,
                    call_id,
                    route,
                    echo_cancellation,
                    g723_bitrate,
                } => {
                    validate_multicast_route(state, route)?;
                    let wire_call_reference = require_call(state, call_id)?.wire_reference;
                    let request = allocate_multicast_request_identity(state)?;
                    let key = MulticastKey {
                        conference_id,
                        call_id,
                    };
                    if let Some(stop) = take_multicast_stop(state, key, true) {
                        send_message(stream, &stop, protocol).await?;
                    }
                    send_message(
                        stream,
                        &ServerMessage::StartMulticastMediaReception(MulticastMediaReception {
                            conference_id,
                            passthrough_party_id: request.token().get().into(),
                            call_reference: CallReference::new(wire_call_reference),
                            address: route.address,
                            port: route.port,
                            packet_millis: route.packet_millis,
                            codec: route.codec,
                            echo_cancellation,
                            g723_bitrate,
                        }),
                        protocol,
                    )
                    .await?;
                    state
                        .multicast
                        .entry(key)
                        .or_insert_with(|| MulticastSession {
                            wire_call_reference,
                            receive: None,
                            transmit: None,
                        })
                        .receive = Some(MulticastReceive {
                        request,
                        route,
                        state: MulticastReceiveState::AwaitingAcknowledgement {
                            deadline: Instant::now() + HANDSET_ACKNOWLEDGEMENT_TIMEOUT,
                        },
                    });
                }
                CommandAction::StopMulticastReception {
                    conference_id,
                    call_id,
                } => {
                    let key = MulticastKey {
                        conference_id,
                        call_id,
                    };
                    if let Some(stop) = take_multicast_stop(state, key, true) {
                        send_message(stream, &stop, protocol).await?;
                    }
                }
                CommandAction::StartMulticastTransmission {
                    conference_id,
                    call_id,
                    route,
                    precedence,
                    silence_suppression,
                    max_frames_per_packet,
                    g723_bitrate,
                } => {
                    validate_multicast_route(state, route)?;
                    let wire_call_reference = require_call(state, call_id)?.wire_reference;
                    let request = allocate_multicast_request_identity(state)?;
                    let key = MulticastKey {
                        conference_id,
                        call_id,
                    };
                    if let Some(stop) = take_multicast_stop(state, key, false) {
                        send_message(stream, &stop, protocol).await?;
                    }
                    send_message(
                        stream,
                        &ServerMessage::StartMulticastMediaTransmission(
                            MulticastMediaTransmission {
                                conference_id,
                                passthrough_party_id: request.token().get().into(),
                                call_reference: CallReference::new(wire_call_reference),
                                address: route.address,
                                port: route.port,
                                packet_millis: route.packet_millis,
                                codec: route.codec,
                                precedence,
                                silence_suppression: silence_suppression.wire_value(),
                                max_frames_per_packet,
                                g723_bitrate,
                            },
                        ),
                        protocol,
                    )
                    .await?;
                    state
                        .multicast
                        .entry(key)
                        .or_insert_with(|| MulticastSession {
                            wire_call_reference,
                            receive: None,
                            transmit: None,
                        })
                        .transmit = Some(MulticastTransmit { request, route });
                    context
                        .event_tx
                        .send(Event::device(
                            state.device.id.clone(),
                            state.generation,
                            DeviceEventKind::MulticastTransmissionStarted {
                                conference_id,
                                call_id,
                                route,
                            },
                        ))
                        .await
                        .map_err(|_| ServerError::Stopped)?;
                }
                CommandAction::StopMulticastTransmission {
                    conference_id,
                    call_id,
                } => {
                    let key = MulticastKey {
                        conference_id,
                        call_id,
                    };
                    if let Some(stop) = take_multicast_stop(state, key, false) {
                        send_message(stream, &stop, protocol).await?;
                    }
                }
                CommandAction::StopMedia { call_id, .. } => {
                    if let Some(call) = state
                        .calls_by_id
                        .get_mut(&call_id)
                        .filter(|call| call.media.transmit.state != MediaChannelState::Closed)
                    {
                        call.media.transmit.state = MediaChannelState::Closed;
                        call.media.transmit.deadline = None;
                        call.media.transmit_confirmation = TransmitConfirmation::Inactive;
                        call.media.coupled_transmit_endpoint = None;
                        let call = call.clone();
                        send_message(
                            stream,
                            &ServerMessage::StopMediaTransmission(AudioStreamControl {
                                conference_id: ConferenceId::new(call.wire_reference),
                                call_reference: CallReference::new(call.wire_reference),
                                passthrough_party_id: media_request_party_id(
                                    call.media.transmit.request,
                                    call.wire_reference,
                                )
                                .into(),
                                port_handling_flag: 0,
                            }),
                            protocol,
                        )
                        .await?;
                    }
                }
                CommandAction::CloseCall { call_id, .. } => {
                    if let Some(call) = state.calls_by_id.get(&call_id).cloned() {
                        let order = *context
                            .call_answer_order
                            .read()
                            .expect("SCCP call-answer-order lock poisoned");
                        let successor = incoming_successor(state, call_id, order);
                        let successor_has_ringer = successor.is_some_and(|(call_id, _)| {
                            state
                                .calls_by_id
                                .get(&call_id)
                                .and_then(|call| incoming_ringer(call.ringer, CallState::RingIn))
                                .is_some_and(ringer_is_audible)
                        });
                        let stop_ringer = !successor_has_ringer
                            && state.ringer_owner.is_none_or(|owner| owner == call_id);
                        state.active_key_mode = KeyMode::OnHook;
                        stop_call_multicast(stream, state, call_id, protocol).await?;
                        if call.state != CallState::OnHook {
                            close_call_media_messages(stream, &call, protocol).await?;
                            close_call_messages(
                                stream,
                                &call,
                                &state.device.soft_keys,
                                protocol,
                                context.config.timezone_offset_minutes,
                                stop_ringer,
                            )
                            .await?;
                            request_connection_statistics(stream, state, &call, context).await?;
                        }
                        remove_call(state, call_id);
                        if state.ringer_owner == Some(call_id) {
                            state.ringer_owner = None;
                        }
                        if let Some((call_id, promote)) = successor {
                            present_incoming_successor(stream, state, call_id, promote).await?;
                        }
                        refresh_mwi_lamps(stream, state, protocol).await?;
                    } else {
                        state.cancelled_calls.insert(call_id);
                    }
                }
            }
        }
    }
    Ok(false)
}

async fn send_mwi_lamp(
    stream: &mut dyn StationIo,
    state: &SessionState,
    line_instance: u32,
    enabled: bool,
    protocol: ProtocolVersion,
) -> Result<(), ServerError> {
    let mode = projected_mwi_lamp(state.device.ui, state.active_call_id.is_some(), enabled);
    send_message(
        stream,
        &ServerMessage::SetLamp {
            stimulus: ButtonType::Voicemail,
            instance: line_instance,
            mode,
        },
        protocol,
    )
    .await
}

fn projected_mwi_lamp(ui: crate::types::StationUiPolicy, on_call: bool, enabled: bool) -> LampMode {
    if enabled && (ui.mwi_on_call || !on_call) {
        ui.mwi_lamp_mode
    } else {
        LampMode::Off
    }
}

fn updated_history_disposition(
    current: CallHistoryDisposition,
    state: CallState,
) -> CallHistoryDisposition {
    if current != CallHistoryDisposition::Missed {
        return current;
    }
    match state {
        CallState::Connected => CallHistoryDisposition::Received,
        CallState::RemoteMultiline => CallHistoryDisposition::Ignore,
        _ => current,
    }
}

async fn refresh_mwi_lamps(
    stream: &mut dyn StationIo,
    state: &SessionState,
    protocol: ProtocolVersion,
) -> Result<(), ServerError> {
    for (&line_instance, &enabled) in &state.mwi_by_line {
        send_mwi_lamp(stream, state, line_instance, enabled, protocol).await?;
    }
    Ok(())
}

fn incoming_ringer(
    ringer: Option<IncomingRing>,
    incoming_state: CallState,
) -> Option<IncomingRing> {
    ringer.map(|mut ringer| {
        if incoming_state == CallState::CallWaiting {
            ringer.duration = RingDuration::Single;
            if ringer.mode != RingerMode::Urgent {
                ringer.mode = RingerMode::Silent;
            }
        }
        ringer
    })
}

const fn ringer_is_audible(ringer: IncomingRing) -> bool {
    !matches!(ringer.mode, RingerMode::Off | RingerMode::Silent)
}

fn incoming_successor(
    state: &SessionState,
    removed_call_id: CallId,
    order: CallSelectionOrder,
) -> Option<(CallId, bool)> {
    let select = |call_state| {
        let candidates = state
            .calls_by_id
            .values()
            .filter(|call| call.call_id != removed_call_id && call.state == call_state);
        match order {
            CallSelectionOrder::OldestFirst => candidates.min_by_key(|call| call.call_id.0),
            CallSelectionOrder::LastFirst => candidates.max_by_key(|call| call.call_id.0),
        }
    };
    if let Some(call) = select(CallState::RingIn) {
        return Some((call.call_id, false));
    }
    let has_active_call = state.calls_by_id.values().any(|call| {
        call.call_id != removed_call_id
            && matches!(
                call.state,
                CallState::Connected | CallState::Hold | CallState::HoldYellow | CallState::HoldRed
            )
    });
    (!has_active_call)
        .then(|| select(CallState::CallWaiting))
        .flatten()
        .map(|call| (call.call_id, true))
}

async fn present_incoming_successor(
    stream: &mut dyn StationIo,
    state: &mut SessionState,
    call_id: CallId,
    promote: bool,
) -> Result<(), ServerError> {
    if promote && let Some(call) = state.calls_by_id.get_mut(&call_id) {
        call.state = CallState::RingIn;
    }
    let call = state
        .calls_by_id
        .get(&call_id)
        .expect("incoming successor came from session state")
        .clone();
    state.active_call_id = Some(call_id);
    state.active_key_mode = KeyMode::RingIn;
    if promote {
        send_message(
            stream,
            &ServerMessage::CallState {
                state: CallState::RingIn,
                line_instance: call.line_instance,
                call_reference: call.wire_reference,
            },
            state.registration.protocol,
        )
        .await?;
    }
    send_message(
        stream,
        &ServerMessage::SetLamp {
            stimulus: ButtonType::Line,
            instance: call.line_instance,
            mode: LampMode::Blink,
        },
        state.registration.protocol,
    )
    .await?;
    if let Some(ringer) = incoming_ringer(call.ringer, CallState::RingIn) {
        let audible = ringer_is_audible(ringer);
        if audible || state.ringer_owner.is_none() {
            send_message(
                stream,
                &ServerMessage::SetRinger {
                    mode: ringer.mode,
                    duration: ringer.duration,
                    line_instance: call.line_instance,
                    call_reference: call.wire_reference,
                },
                state.registration.protocol,
            )
            .await?;
        }
        if audible {
            state.ringer_owner = Some(call_id);
        }
    }
    send_message(
        stream,
        &ServerMessage::SelectSoftKeys {
            line_instance: call.line_instance,
            call_reference: call.wire_reference,
            set: KeyMode::RingIn,
            valid_mask: state.device.soft_keys.valid_mask(KeyMode::RingIn),
        },
        state.registration.protocol,
    )
    .await?;
    Ok(())
}

async fn request_connection_statistics(
    stream: &mut dyn StationIo,
    state: &mut SessionState,
    call: &SessionCall,
    context: &SessionContext,
) -> Result<(), ServerError> {
    prune_connection_statistics(&mut state.pending_connection_statistics, Instant::now());
    if !call.media.requested
        || state.pending_connection_statistics.len() >= MAX_PENDING_CONNECTION_STATISTICS
        || state.statistics_references.len() >= MAX_STATISTICS_REFERENCES_PER_SESSION
    {
        return Ok(());
    }
    let directory_number = if call.statistics_directory_number.is_empty() {
        call.dialed_number.trim()
    } else {
        call.statistics_directory_number.trim()
    };
    let maximum = if state.registration.protocol >= ProtocolVersion::V19 {
        24
    } else {
        23
    };
    if directory_number.is_empty()
        || directory_number.len() > maximum
        || directory_number.contains(['\0', '\r', '\n'])
    {
        warn!(
            device_id = %state.device.id,
            ?call.call_id,
            byte_count = directory_number.len(),
            "skipping connection-statistics request with unusable directory number"
        );
        return Ok(());
    }
    if !state.statistics_references.insert(call.wire_reference) {
        warn!(
            device_id = %state.device.id,
            ?call.call_id,
            call_reference = call.wire_reference,
            "skipping connection-statistics request for a reused call reference"
        );
        return Ok(());
    }
    let request_generation = context
        .next_statistics_generation
        .fetch_add(1, Ordering::Relaxed);
    let processing = StatisticsProcessing::Clear;
    let session_generation = state.generation;
    state.pending_connection_statistics.insert(
        call.wire_reference,
        PendingConnectionStatistics {
            session_generation,
            request_generation,
            call_id: call.call_id,
            line_instance: call.line_instance,
            codec: call.media.codec,
            packet_ms: call.media.packet_ms,
            max_frames_per_packet: call.media.max_frames_per_packet,
            receive_peer: call.media.receive.peer,
            transmit_peer: call.media.transmit.peer,
            directory_number: directory_number.to_owned(),
            processing,
            expires_at: Instant::now() + CONNECTION_STATISTICS_TIMEOUT,
        },
    );
    send_message(
        stream,
        &ServerMessage::ConnectionStatisticsRequest {
            directory_number: directory_number.to_owned(),
            call_reference: call.wire_reference,
            processing,
        },
        state.registration.protocol,
    )
    .await
}

fn statistics_directory_for_call_info(info: &CallInfo) -> &str {
    match info.direction {
        crate::types::CallDirection::Inbound => &info.calling_number,
        crate::types::CallDirection::Outbound => &info.called_number,
    }
}

fn prune_connection_statistics(
    pending_statistics: &mut HashMap<u32, PendingConnectionStatistics>,
    now: Instant,
) {
    pending_statistics.retain(|_, pending| pending.expires_at > now);
}

async fn collect_connection_statistics(
    state: &mut SessionState,
    statistics: ConnectionStatistics,
    context: &SessionContext,
) -> Result<(), ServerError> {
    prune_connection_statistics(&mut state.pending_connection_statistics, Instant::now());
    let Some(pending) = state
        .pending_connection_statistics
        .get(&statistics.call_reference)
        .cloned()
    else {
        warn!(
            device_id = %state.device.id,
            call_reference = statistics.call_reference,
            "ignoring unsolicited or expired connection-statistics response"
        );
        return Ok(());
    };
    let current_session = context
        .sessions
        .lock()
        .await
        .get(&state.device.id)
        .is_some_and(|session| session.generation == pending.session_generation);
    if !current_session
        || pending.session_generation != state.generation
        || statistics.processing != pending.processing
        || statistics.directory_number != pending.directory_number
    {
        warn!(
            device_id = %state.device.id,
            call_reference = statistics.call_reference,
            processing = ?statistics.processing,
            "ignoring mismatched connection-statistics response"
        );
        return Ok(());
    }
    state
        .pending_connection_statistics
        .remove(&statistics.call_reference);
    let snapshot = MediaStatisticsSnapshot {
        request_generation: pending.request_generation,
        call_id: pending.call_id,
        line_instance: LineInstance::new(pending.line_instance),
        codec: pending.codec,
        packet_ms: pending.packet_ms,
        max_frames_per_packet: pending.max_frames_per_packet,
        receive_peer: pending.receive_peer,
        transmit_peer: pending.transmit_peer,
        packets_sent: statistics.packets_sent,
        octets_sent: statistics.octets_sent,
        packets_received: statistics.packets_received,
        octets_received: statistics.octets_received,
        packets_lost: statistics.packets_lost,
        jitter_millis: statistics.jitter_millis,
        latency_millis: statistics.latency_millis,
        quality_byte_count: statistics.quality.as_bytes().len(),
    };
    {
        let mut latest = context
            .latest_media_statistics
            .write()
            .expect("SCCP media-statistics lock poisoned");
        let replace = latest
            .get(&state.device.id)
            .is_none_or(|existing| existing.request_generation < snapshot.request_generation);
        if !replace {
            return Ok(());
        }
        latest.insert(state.device.id.clone(), snapshot.clone());
    }
    context
        .event_tx
        .send(Event::device(
            state.device.id.clone(),
            state.generation,
            DeviceEventKind::ConnectionStatisticsCollected { snapshot },
        ))
        .await
        .map_err(|_| ServerError::Stopped)
}

fn status_message_frames(
    message: HandsetStatusMessage,
    device_type: DeviceType,
    persistent: &mut bool,
) -> Vec<ServerMessage> {
    let prompt_for_timed_message = matches!(
        device_type,
        DeviceType::Cisco6901
            | DeviceType::Cisco6921
            | DeviceType::Cisco6941
            | DeviceType::Cisco6945
            | DeviceType::Cisco6961
    );
    match message {
        HandsetStatusMessage::Display {
            text,
            timeout_seconds,
            priority: Some(priority),
        } => vec![ServerMessage::DisplayPriorityNotify {
            timeout_seconds: u32::from(timeout_seconds),
            priority,
            text,
        }],
        HandsetStatusMessage::Clear {
            priority: Some(priority),
        } => vec![ServerMessage::ClearPriorityNotify { priority }],
        HandsetStatusMessage::Display {
            text,
            timeout_seconds,
            priority: None,
        } if timeout_seconds == 0 || prompt_for_timed_message => {
            if timeout_seconds == 0 {
                *persistent = true;
            }
            vec![ServerMessage::DisplayPrompt {
                timeout_seconds: u32::from(timeout_seconds),
                text,
                line_instance: 0,
                call_reference: 0,
            }]
        }
        HandsetStatusMessage::Display {
            text,
            timeout_seconds,
            priority: None,
        } => vec![ServerMessage::DisplayPriorityNotify {
            timeout_seconds: u32::from(timeout_seconds),
            priority: NotificationPriority::Timed,
            text,
        }],
        HandsetStatusMessage::Clear { priority: None } => {
            let clear_prompt = std::mem::take(persistent) || prompt_for_timed_message;
            let mut frames = Vec::with_capacity(2);
            if clear_prompt {
                frames.push(ServerMessage::ClearPrompt {
                    line_instance: 0,
                    call_reference: 0,
                });
            }
            if !prompt_for_timed_message {
                frames.push(ServerMessage::ClearPriorityNotify {
                    priority: NotificationPriority::Timed,
                });
            }
            frames
        }
    }
}

async fn send_message(
    stream: &mut dyn StationIo,
    message: &ServerMessage,
    session: impl Into<StationSessionContext>,
) -> Result<(), ServerError> {
    stream
        .write_all(&message.encode_for_session(session.into())?)
        .await?;
    Ok(())
}

async fn send_station_ui_message(
    stream: &mut dyn StationIo,
    state: &SessionState,
    message: &ServerMessage,
) -> Result<(), ServerError> {
    let session = state.station_context();
    let bytes = if state.features.contains(PhoneFeatures::UTF8) {
        message.encode_for_session(session)?
    } else {
        message.encode_for_legacy_session(session, state.device.ui.legacy_code_page)?
    };
    stream.write_all(&bytes).await?;
    Ok(())
}

async fn begin_phone_call_ui(
    stream: &mut dyn StationIo,
    call: &SessionCall,
    device: &DeviceDefinition,
    session: StationSessionContext,
) -> Result<(), ServerError> {
    begin_phone_call_ui_with_key_mode(stream, call, device, KeyMode::OffHook, session).await
}

async fn begin_phone_call_ui_with_key_mode(
    stream: &mut dyn StationIo,
    call: &SessionCall,
    device: &DeviceDefinition,
    key_mode: KeyMode,
    session: StationSessionContext,
) -> Result<(), ServerError> {
    let initial_tone = device
        .line(call.line_instance)
        .map_or(Tone::InsideDial, |line| line.initial_tone);
    send_message(
        stream,
        &ServerMessage::SetSpeakerMode(SpeakerMode::On),
        session,
    )
    .await?;
    send_message(
        stream,
        &ServerMessage::SetLamp {
            stimulus: ButtonType::Line,
            instance: call.line_instance,
            mode: LampMode::On,
        },
        session,
    )
    .await?;
    send_message(
        stream,
        &ServerMessage::CallState {
            state: CallState::OffHook,
            line_instance: call.line_instance,
            call_reference: call.wire_reference,
        },
        session,
    )
    .await?;
    send_message(
        stream,
        &ServerMessage::ActivateCallPlane {
            line_instance: call.line_instance,
        },
        session,
    )
    .await?;
    send_message(
        stream,
        &ServerMessage::DisplayPrompt {
            timeout_seconds: 0,
            text: "Enter number".into(),
            line_instance: call.line_instance,
            call_reference: call.wire_reference,
        },
        session,
    )
    .await?;
    send_message(
        stream,
        &ServerMessage::StartTone {
            tone: initial_tone,
            direction: ToneDirection::User,
            line_instance: call.line_instance,
            call_reference: call.wire_reference,
        },
        session,
    )
    .await?;
    send_message(
        stream,
        &ServerMessage::SelectSoftKeys {
            line_instance: call.line_instance,
            call_reference: call.wire_reference,
            set: key_mode,
            valid_mask: device.soft_keys.valid_mask(key_mode),
        },
        session,
    )
    .await
}

async fn begin_answer_ui(
    stream: &mut dyn StationIo,
    call: &SessionCall,
    protocol: ProtocolVersion,
) -> Result<(), ServerError> {
    send_message(
        stream,
        &ServerMessage::SetRinger {
            mode: RingerMode::Off,
            duration: RingDuration::Normal,
            line_instance: call.line_instance,
            call_reference: call.wire_reference,
        },
        protocol,
    )
    .await?;
    send_message(
        stream,
        &ServerMessage::CallState {
            state: CallState::OffHook,
            line_instance: call.line_instance,
            call_reference: call.wire_reference,
        },
        protocol,
    )
    .await?;
    send_message(
        stream,
        &ServerMessage::ActivateCallPlane {
            line_instance: call.line_instance,
        },
        protocol,
    )
    .await?;
    send_message(
        stream,
        &ServerMessage::StopTone {
            line_instance: call.line_instance,
            call_reference: call.wire_reference,
        },
        protocol,
    )
    .await?;
    send_message(
        stream,
        &ServerMessage::SetLamp {
            stimulus: ButtonType::Line,
            instance: call.line_instance,
            mode: LampMode::On,
        },
        protocol,
    )
    .await?;
    Ok(())
}

async fn prepare_call_state_ui(
    stream: &mut dyn StationIo,
    call: &SessionCall,
    state: CallState,
    protocol: ProtocolVersion,
) -> Result<(), ServerError> {
    match state {
        CallState::Connected => {
            send_message(
                stream,
                &ServerMessage::SetRinger {
                    mode: RingerMode::Off,
                    duration: RingDuration::Normal,
                    line_instance: call.line_instance,
                    call_reference: call.wire_reference,
                },
                protocol,
            )
            .await?;
            send_message(
                stream,
                &ServerMessage::SetSpeakerMode(SpeakerMode::On),
                protocol,
            )
            .await?;
            send_message(
                stream,
                &ServerMessage::StopTone {
                    line_instance: call.line_instance,
                    call_reference: call.wire_reference,
                },
                protocol,
            )
            .await?;
            send_message(
                stream,
                &ServerMessage::SetLamp {
                    stimulus: ButtonType::Line,
                    instance: call.line_instance,
                    mode: LampMode::On,
                },
                protocol,
            )
            .await?;
        }
        CallState::RemoteMultiline => {
            send_message(
                stream,
                &ServerMessage::SetRinger {
                    mode: RingerMode::Off,
                    duration: RingDuration::Normal,
                    line_instance: call.line_instance,
                    call_reference: call.wire_reference,
                },
                protocol,
            )
            .await?;
            send_message(
                stream,
                &ServerMessage::SetSpeakerMode(SpeakerMode::Off),
                protocol,
            )
            .await?;
            send_message(
                stream,
                &ServerMessage::SetLamp {
                    stimulus: ButtonType::Line,
                    instance: call.line_instance,
                    mode: LampMode::On,
                },
                protocol,
            )
            .await?;
        }
        CallState::OnHook => {
            send_message(
                stream,
                &ServerMessage::SetRinger {
                    mode: RingerMode::Off,
                    duration: RingDuration::Normal,
                    line_instance: call.line_instance,
                    call_reference: call.wire_reference,
                },
                protocol,
            )
            .await?;
        }
        CallState::Hold | CallState::HoldYellow | CallState::HoldRed => {
            send_message(
                stream,
                &ServerMessage::SetLamp {
                    stimulus: ButtonType::Line,
                    instance: call.line_instance,
                    mode: LampMode::Wink,
                },
                protocol,
            )
            .await?;
        }
        CallState::RingOut | CallState::Proceed => {
            send_message(
                stream,
                &ServerMessage::SetLamp {
                    stimulus: ButtonType::Line,
                    instance: call.line_instance,
                    mode: LampMode::Blink,
                },
                protocol,
            )
            .await?;
        }
        _ => {}
    }
    Ok(())
}

async fn finish_call_state_ui(
    stream: &mut dyn StationIo,
    call: &SessionCall,
    state: CallState,
    session: StationSessionContext,
) -> Result<(), ServerError> {
    let prompt = match state {
        CallState::Connected => Some("Connected"),
        CallState::Hold | CallState::HoldYellow | CallState::HoldRed => Some("Hold"),
        CallState::RingOut => Some("Ring out"),
        CallState::Proceed => Some("Call proceeding"),
        CallState::Busy => Some("Busy"),
        CallState::Congestion => Some("Network congestion"),
        CallState::InvalidNumber => Some("Unknown number"),
        _ => None,
    };
    if state == CallState::Connected {
        send_message(
            stream,
            &ServerMessage::ActivateCallPlane {
                line_instance: call.line_instance,
            },
            session,
        )
        .await?;
    } else if matches!(
        state,
        CallState::Hold | CallState::HoldYellow | CallState::HoldRed
    ) {
        send_message(
            stream,
            &ServerMessage::SetSpeakerMode(SpeakerMode::Off),
            session,
        )
        .await?;
    }
    if let Some(text) = prompt {
        send_message(
            stream,
            &ServerMessage::DisplayPrompt {
                timeout_seconds: 0,
                text: text.into(),
                line_instance: call.line_instance,
                call_reference: call.wire_reference,
            },
            session,
        )
        .await?;
    }
    Ok(())
}

fn normalize_line(state: &SessionState, requested: u32) -> u32 {
    if requested != 0 && state.device.line(requested).is_some() {
        requested
    } else {
        state.device.first_line().map_or(1, |line| line.instance)
    }
}

fn ensure_phone_call(
    state: &mut SessionState,
    wire_reference: u32,
    line_instance: u32,
    next: &AtomicU64,
) -> SessionCall {
    let reusable = if wire_reference == 0 {
        state
            .calls_by_id
            .values()
            .filter(|call| call.state != CallState::OnHook)
            .max_by_key(|call| call.call_id.0)
    } else {
        find_call(state, wire_reference).filter(|call| call.state != CallState::OnHook)
    };
    if let Some(call) = reusable {
        return call.clone();
    }
    let mut call = reserve_phone_call(state, line_instance, next);
    if wire_reference != 0
        && wire_reference != call.wire_reference
        && !state.statistics_references.contains(&wire_reference)
    {
        state.calls_by_wire.remove(&call.wire_reference);
        call.wire_reference = wire_reference;
        state.calls_by_wire.insert(wire_reference, call.call_id);
        state.calls_by_id.insert(call.call_id, call.clone());
    }
    call
}

fn reserve_phone_call(
    state: &mut SessionState,
    line_instance: u32,
    next: &AtomicU64,
) -> SessionCall {
    let call_id = CallId(next.fetch_add(1, Ordering::Relaxed));
    insert_call(
        state,
        call_id,
        line_instance,
        Codec::Pcmu,
        CallState::OffHook,
    )
}

fn insert_call(
    state: &mut SessionState,
    call_id: CallId,
    line_instance: u32,
    codec: Codec,
    call_state: CallState,
) -> SessionCall {
    let mut wire_reference = (call_id.0 as u32).max(1);
    while state.calls_by_wire.contains_key(&wire_reference)
        || state.statistics_references.contains(&wire_reference)
    {
        wire_reference = wire_reference.wrapping_add(1).max(1);
    }
    let call = SessionCall {
        call_id,
        wire_reference,
        line_instance,
        media: CallMedia::new(codec),
        video_receive: VideoReceive::default(),
        video_transmit: VideoTransmit::default(),
        state: call_state,
        ringer: None,
        history_disposition: if matches!(call_state, CallState::RingIn | CallState::CallWaiting) {
            CallHistoryDisposition::Missed
        } else {
            CallHistoryDisposition::Placed
        },
        dialed_number: String::new(),
        statistics_directory_number: String::new(),
        transfer_role: None,
    };
    state.calls_by_wire.insert(wire_reference, call_id);
    state.calls_by_id.insert(call_id, call.clone());
    call
}

fn find_call(state: &SessionState, wire_reference: u32) -> Option<&SessionCall> {
    if wire_reference != 0 {
        state
            .calls_by_wire
            .get(&wire_reference)
            .and_then(|id| state.calls_by_id.get(id))
    } else {
        state
            .active_call_id
            .and_then(|call_id| state.calls_by_id.get(&call_id))
            .or_else(|| {
                (state.calls_by_id.len() == 1)
                    .then(|| state.calls_by_id.values().next())
                    .flatten()
            })
    }
}

fn find_answer_call(
    state: &SessionState,
    wire_reference: u32,
    line_instance: u32,
    order: CallSelectionOrder,
) -> Option<&SessionCall> {
    let matches_line = |call: &&SessionCall| {
        matches!(call.state, CallState::RingIn | CallState::CallWaiting)
            && (line_instance == 0 || call.line_instance == line_instance)
    };
    if wire_reference != 0 {
        return state
            .calls_by_wire
            .get(&wire_reference)
            .and_then(|call_id| state.calls_by_id.get(call_id))
            .filter(matches_line);
    }
    if let Some(active) = state
        .active_call_id
        .and_then(|call_id| state.calls_by_id.get(&call_id))
        .filter(matches_line)
    {
        return Some(active);
    }
    let candidates = state.calls_by_id.values().filter(matches_line);
    match order {
        CallSelectionOrder::OldestFirst => candidates.min_by_key(|call| call.call_id.0),
        CallSelectionOrder::LastFirst => candidates.max_by_key(|call| call.call_id.0),
    }
}

fn find_receive_media_call_id(
    state: &SessionState,
    wire_reference: u32,
    passthrough_party_id: u32,
) -> Option<CallId> {
    find_media_call_id(state, wire_reference, passthrough_party_id, |call| {
        call.media.receive.request
    })
}

fn find_multicast_receive_key(
    state: &SessionState,
    wire_reference: u32,
    passthrough_party_id: u32,
) -> Option<MulticastKey> {
    state.multicast.iter().find_map(|(key, session)| {
        session.receive.as_ref().and_then(|receive| {
            (matches!(
                receive.state,
                MulticastReceiveState::AwaitingAcknowledgement { .. }
            ) && session.wire_call_reference == wire_reference
                && receive.request.token().get() == passthrough_party_id)
                .then_some(*key)
        })
    })
}

fn find_multicast_transmit_key(
    state: &SessionState,
    conference_id: u32,
    wire_reference: u32,
    passthrough_party_id: u32,
    address: IpAddr,
    port: u16,
) -> Option<MulticastKey> {
    state.multicast.iter().find_map(|(key, session)| {
        session.transmit.as_ref().and_then(|transmit| {
            (key.conference_id.get() == conference_id
                && session.wire_call_reference == wire_reference
                && transmit.request.token().get() == passthrough_party_id
                && canonical_ip_address(transmit.route.address) == canonical_ip_address(address)
                && transmit.route.port == port)
                .then_some(*key)
        })
    })
}

fn find_transmit_media_call_id(
    state: &SessionState,
    conference_id: u32,
    wire_reference: u32,
    passthrough_party_id: u32,
) -> Option<CallId> {
    find_media_call_id(state, wire_reference, passthrough_party_id, |call| {
        call.media.transmit.request
    })
    .filter(|call_id| {
        state
            .calls_by_id
            .get(call_id)
            .is_some_and(|call| conference_id == 0 || conference_id == call.wire_reference)
    })
}

fn find_media_call_id(
    state: &SessionState,
    wire_reference: u32,
    passthrough_party_id: u32,
    request: impl Fn(&SessionCall) -> Option<MediaRequestIdentity>,
) -> Option<CallId> {
    state
        .calls_by_id
        .values()
        .find(|call| {
            request(call).is_some_and(|identity| {
                identity.accepts_ack(passthrough_party_id, wire_reference, call.wire_reference)
            })
        })
        .map(|call| call.call_id)
}

fn require_call(state: &SessionState, call_id: CallId) -> Result<&SessionCall, ServerError> {
    state
        .calls_by_id
        .get(&call_id)
        .ok_or(ServerError::UnknownCall(call_id))
}

fn require_call_mut(
    state: &mut SessionState,
    call_id: CallId,
) -> Result<&mut SessionCall, ServerError> {
    state
        .calls_by_id
        .get_mut(&call_id)
        .ok_or(ServerError::UnknownCall(call_id))
}

fn address_matches_type(address: IpAddr, requested: IpAddressType) -> bool {
    match requested {
        IpAddressType::Ipv4 => address.is_ipv4(),
        IpAddressType::Ipv6 => address.is_ipv6(),
        IpAddressType::Ipv4AndIpv6 => true,
        IpAddressType::Invalid | IpAddressType::Unknown(_) => false,
    }
}

fn address_type(address: IpAddr) -> IpAddressType {
    if address.is_ipv4() {
        IpAddressType::Ipv4
    } else {
        IpAddressType::Ipv6
    }
}

fn endpoint_is_usable(endpoint: MediaEndpointAddress) -> bool {
    endpoint.port != 0 && !endpoint.address.is_unspecified() && !endpoint.address.is_multicast()
}

fn capability_supports_address(
    advertised: Option<IpAddressType>,
    requested: IpAddressType,
) -> bool {
    match advertised {
        None => requested == IpAddressType::Ipv4,
        Some(IpAddressType::Ipv4AndIpv6) => true,
        Some(address_type) => address_type == requested,
    }
}

fn validate_multimedia_receive_descriptor(
    descriptor: &MultimediaReceiveDescriptor,
) -> Result<(), ServerError> {
    if !descriptor
        .payload
        .is_direction(MultimediaPayloadDirection::Receive)
    {
        return Err(ServerError::InvalidMultimediaReceive(
            "payload was not decoded from a receive message",
        ));
    }
    if descriptor.payload.codec().kind() != CodecKind::Video {
        return Err(ServerError::InvalidMultimediaReceive("codec is not video"));
    }
    if !address_matches_type(descriptor.source.address, descriptor.requested_address_type) {
        return Err(ServerError::InvalidMultimediaReceive(
            "source address does not match the requested address type",
        ));
    }
    if descriptor.source.address.is_multicast() {
        return Err(ServerError::InvalidMultimediaReceive(
            "source address must not be multicast",
        ));
    }
    Ok(())
}

fn validate_multimedia_receive(
    state: &SessionState,
    descriptor: &MultimediaReceiveDescriptor,
) -> Result<(), ServerError> {
    validate_multimedia_receive_descriptor(descriptor)?;

    if !descriptor.payload.is_valid_for(
        MultimediaPayloadDirection::Receive,
        state.registration.protocol,
    ) {
        return Err(ServerError::InvalidMultimediaReceive(
            "payload protocol does not match the live session",
        ));
    }

    match state.registration.protocol {
        protocol if protocol < ProtocolVersion::V12 => {
            if descriptor.source
                != (MediaEndpointAddress {
                    address: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    port: 0,
                })
                || descriptor.requested_address_type != IpAddressType::Ipv4
            {
                return Err(ServerError::InvalidMultimediaReceive(
                    "this protocol version cannot carry a source endpoint",
                ));
            }
        }
        protocol
            if protocol < ProtocolVersion::V17
                && (!descriptor.source.address.is_ipv4()
                    || descriptor.requested_address_type != IpAddressType::Ipv4) =>
        {
            return Err(ServerError::InvalidMultimediaReceive(
                "this protocol version carries only IPv4 video endpoints",
            ));
        }
        _ => {}
    }

    let supported = state.media_capabilities.video().iter().any(|capability| {
        let encryption_supported = descriptor.encryption.is_none()
            || capability.encryption_capability == Some(EncryptionCapability::Capable);
        capability.codec == descriptor.payload.codec()
            && capability.direction.contains(ReceiveTransmit::RECEIVE)
            && capability_supports_address(
                capability.address_type,
                descriptor.requested_address_type,
            )
            && encryption_supported
    });
    supported
        .then_some(())
        .ok_or(ServerError::UnsupportedMultimediaReceive)
}

fn validate_multimedia_transmit_descriptor(
    descriptor: &MultimediaTransmitDescriptor,
) -> Result<(), ServerError> {
    if !descriptor
        .payload
        .is_direction(MultimediaPayloadDirection::Transmit)
    {
        return Err(ServerError::InvalidMultimediaTransmit(
            "payload was not decoded from a transmit message",
        ));
    }
    if descriptor.payload.codec().kind() != CodecKind::Video {
        return Err(ServerError::InvalidMultimediaTransmit("codec is not video"));
    }
    if !endpoint_is_usable(descriptor.endpoint) {
        return Err(ServerError::InvalidMultimediaTransmit(
            "destination endpoint must be unicast and nonzero",
        ));
    }
    Ok(())
}

fn validate_multimedia_transmit(
    state: &SessionState,
    descriptor: &MultimediaTransmitDescriptor,
) -> Result<(), ServerError> {
    validate_multimedia_transmit_descriptor(descriptor)?;
    if !descriptor.payload.is_valid_for(
        MultimediaPayloadDirection::Transmit,
        state.registration.protocol,
    ) {
        return Err(ServerError::InvalidMultimediaTransmit(
            "payload protocol does not match the live session",
        ));
    }
    if state.registration.protocol < ProtocolVersion::V17 && descriptor.endpoint.address.is_ipv6() {
        return Err(ServerError::InvalidMultimediaTransmit(
            "this protocol version carries only IPv4 video endpoints",
        ));
    }
    let requested_address = address_type(descriptor.endpoint.address);
    let supported = state.media_capabilities.video().iter().any(|capability| {
        let encryption_supported = descriptor.encryption.is_none()
            || capability.encryption_capability == Some(EncryptionCapability::Capable);
        capability.codec == descriptor.payload.codec()
            && capability.direction.contains(ReceiveTransmit::TRANSMIT)
            && capability_supports_address(capability.address_type, requested_address)
            && encryption_supported
    });
    supported
        .then_some(())
        .ok_or(ServerError::UnsupportedMultimediaTransmit)
}

fn allocate_video_receive_identity(
    state: &mut SessionState,
    call_id: CallId,
) -> Result<MediaRequestIdentity, ServerError> {
    let generation = require_call(state, call_id)?
        .video_receive
        .generation
        .checked_add(1)
        .ok_or(ServerError::MediaRequestIdentityExhausted)?;
    let token = state
        .next_media_token
        .ok_or(ServerError::MediaRequestIdentityExhausted)?;
    let request = MediaRequestIdentity::new(generation, token)
        .ok_or(ServerError::MediaRequestIdentityExhausted)?;
    state.next_media_token = token.checked_next();
    require_call_mut(state, call_id)?.video_receive.generation = generation;
    Ok(request)
}

fn multimedia_receive_close_message(call: &SessionCall, leg: &VideoReceiveLeg) -> ServerMessage {
    ServerMessage::CloseMultimediaReceiveChannel(MultimediaStreamControl {
        conference_id: leg.conference_id,
        passthrough_party_id: leg.request.token().get().into(),
        call_reference: CallReference::new(call.wire_reference),
        port_handling_flag: 0,
    })
}

fn take_multimedia_receive_close(
    state: &mut SessionState,
    call_id: CallId,
) -> Option<ServerMessage> {
    let call = state.calls_by_id.get_mut(&call_id)?;
    let leg = call.video_receive.leg.take()?;
    Some(multimedia_receive_close_message(call, &leg))
}

fn take_all_multimedia_receive_closes(state: &mut SessionState) -> Vec<ServerMessage> {
    let mut call_ids = state.calls_by_id.keys().copied().collect::<Vec<_>>();
    call_ids.sort_unstable_by_key(|call_id| call_id.get());
    call_ids
        .into_iter()
        .filter_map(|call_id| take_multimedia_receive_close(state, call_id))
        .collect()
}

fn expire_multimedia_receive_acknowledgements(
    state: &mut SessionState,
    now: Instant,
) -> Vec<ExpiredVideoReceive> {
    let mut call_ids = state
        .calls_by_id
        .iter()
        .filter_map(|(&call_id, call)| {
            call.video_receive.leg.as_ref().and_then(|leg| {
                (leg.state == MediaChannelState::Opening
                    && leg.deadline.is_some_and(|deadline| deadline <= now))
                .then_some(call_id)
            })
        })
        .collect::<Vec<_>>();
    call_ids.sort_unstable_by_key(|call_id| call_id.get());
    call_ids
        .into_iter()
        .filter_map(|call_id| {
            let leg = state
                .calls_by_id
                .get(&call_id)?
                .video_receive
                .leg
                .as_ref()?;
            let codec = leg.codec;
            let passthrough_party_id = leg.request.token().get().into();
            take_multimedia_receive_close(state, call_id).map(|close| ExpiredVideoReceive {
                call_id,
                codec,
                passthrough_party_id,
                close,
            })
        })
        .collect()
}

fn allocate_video_transmit_identity(
    state: &mut SessionState,
    call_id: CallId,
) -> Result<MediaRequestIdentity, ServerError> {
    let generation = require_call(state, call_id)?
        .video_transmit
        .generation
        .checked_add(1)
        .ok_or(ServerError::MediaRequestIdentityExhausted)?;
    let token = state
        .next_media_token
        .ok_or(ServerError::MediaRequestIdentityExhausted)?;
    let request = MediaRequestIdentity::new(generation, token)
        .ok_or(ServerError::MediaRequestIdentityExhausted)?;
    state.next_media_token = token.checked_next();
    require_call_mut(state, call_id)?.video_transmit.generation = generation;
    Ok(request)
}

fn multimedia_transmit_control_identity(
    state: &SessionState,
    call_id: CallId,
    passthrough_party_id: PassthroughPartyId,
) -> Result<(ConferenceId, CallReference), ServerError> {
    let call = require_call(state, call_id)?;
    if call.state != CallState::Connected {
        return Err(ServerError::InvalidCallTransaction {
            call_id,
            operation: "control video transmit media",
            state: call.state,
        });
    }
    let leg = call
        .video_transmit
        .leg
        .as_ref()
        .filter(|leg| {
            leg.state == MediaChannelState::Open
                && leg.request.token().get() == passthrough_party_id.get()
        })
        .ok_or(ServerError::StaleMultimediaTransmitControl {
            call_id,
            passthrough_party_id,
        })?;
    Ok((leg.conference_id, CallReference::new(call.wire_reference)))
}

fn encode_multimedia_transmit_control(
    control: MultimediaTransmitControl,
) -> Result<(MiscCommandType, BoundedBytes<36>), ServerError> {
    let (command, words) = match control {
        MultimediaTransmitControl::FreezePicture => {
            (MiscCommandType::VideoFreezePicture, Vec::new())
        }
        MultimediaTransmitControl::FastPictureUpdate {
            first_gob,
            gob_count,
        } => (
            MiscCommandType::VideoFastUpdatePicture,
            vec![first_gob, gob_count],
        ),
        MultimediaTransmitControl::FastGobUpdate {
            first_gob,
            gob_count,
        } => (
            MiscCommandType::VideoFastUpdateGob,
            vec![first_gob, gob_count],
        ),
        MultimediaTransmitControl::FastMacroblockUpdate {
            first_gob,
            first_macroblock,
            macroblock_count,
        } => (
            MiscCommandType::VideoFastUpdateMacroblock,
            vec![first_gob, first_macroblock, macroblock_count],
        ),
        MultimediaTransmitControl::LostPicture {
            picture_number,
            long_term_picture_index,
        } => (
            MiscCommandType::LostPicture,
            vec![picture_number, long_term_picture_index],
        ),
        MultimediaTransmitControl::LostPartialPicture {
            picture_number,
            long_term_picture_index,
            first_macroblock,
            macroblock_count,
        } => (
            MiscCommandType::LostPartialPicture,
            vec![
                picture_number,
                long_term_picture_index,
                first_macroblock,
                macroblock_count,
            ],
        ),
        MultimediaTransmitControl::RecoveryReferencePicture { pictures } => {
            let words =
                std::iter::once(pictures.as_slice().len() as u32)
                    .chain(pictures.as_slice().iter().flat_map(|picture| {
                        [picture.picture_number, picture.long_term_picture_index]
                    }))
                    .collect();
            (MiscCommandType::RecoveryReferencePicture, words)
        }
        MultimediaTransmitControl::TemporalSpatialTradeoff { value } => {
            (MiscCommandType::TemporalSpatialTradeoff, vec![value])
        }
    };
    let data = words
        .into_iter()
        .flat_map(u32::to_le_bytes)
        .collect::<Vec<_>>();
    let data = BoundedBytes::new(data.into_boxed_slice()).map_err(|_| {
        ServerError::InvalidMultimediaTransmitControl("parameter area exceeds 36 bytes")
    })?;
    Ok((command, data))
}

fn multimedia_transmit_stop_message(call: &SessionCall, leg: &VideoTransmitLeg) -> ServerMessage {
    ServerMessage::StopMultimediaTransmission(MultimediaStreamControl {
        conference_id: leg.conference_id,
        passthrough_party_id: leg.request.token().get().into(),
        call_reference: CallReference::new(call.wire_reference),
        port_handling_flag: 0,
    })
}

fn take_multimedia_transmit_stop(
    state: &mut SessionState,
    call_id: CallId,
) -> Option<ServerMessage> {
    let call = state.calls_by_id.get_mut(&call_id)?;
    let leg = call.video_transmit.leg.take()?;
    Some(multimedia_transmit_stop_message(call, &leg))
}

fn take_all_multimedia_transmit_stops(state: &mut SessionState) -> Vec<ServerMessage> {
    let mut call_ids = state.calls_by_id.keys().copied().collect::<Vec<_>>();
    call_ids.sort_unstable_by_key(|call_id| call_id.get());
    call_ids
        .into_iter()
        .filter_map(|call_id| take_multimedia_transmit_stop(state, call_id))
        .collect()
}

fn expire_multimedia_transmit_acknowledgements(
    state: &mut SessionState,
    now: Instant,
) -> Vec<ExpiredVideoTransmit> {
    let mut call_ids = state
        .calls_by_id
        .iter()
        .filter_map(|(&call_id, call)| {
            call.video_transmit.leg.as_ref().and_then(|leg| {
                (leg.state == MediaChannelState::Opening
                    && leg.deadline.is_some_and(|deadline| deadline <= now))
                .then_some(call_id)
            })
        })
        .collect::<Vec<_>>();
    call_ids.sort_unstable_by_key(|call_id| call_id.get());
    call_ids
        .into_iter()
        .filter_map(|call_id| {
            let leg = state
                .calls_by_id
                .get(&call_id)?
                .video_transmit
                .leg
                .as_ref()?;
            let codec = leg.codec;
            let passthrough_party_id = leg.request.token().get().into();
            take_multimedia_transmit_stop(state, call_id).map(|stop| ExpiredVideoTransmit {
                call_id,
                codec,
                passthrough_party_id,
                stop,
            })
        })
        .collect()
}

fn validate_multicast_route(
    state: &SessionState,
    route: MulticastMediaRoute,
) -> Result<(), ServerError> {
    if !route.address.is_multicast() {
        return Err(ServerError::InvalidMulticastMedia(
            "address must be multicast",
        ));
    }
    if route.address.is_ipv6() && state.registration.protocol < ProtocolVersion::V17 {
        return Err(ServerError::InvalidMulticastMedia(
            "IPv6 requires protocol v17 or later",
        ));
    }
    if route.port == 0 {
        return Err(ServerError::InvalidMulticastMedia("port must be nonzero"));
    }
    if route.packet_millis == 0 {
        return Err(ServerError::InvalidMulticastMedia(
            "packet duration must be nonzero",
        ));
    }
    if route.codec.kind() != CodecKind::Audio {
        return Err(ServerError::UnsupportedMulticastCodec);
    }
    let capability = state
        .media_capabilities
        .audio()
        .iter()
        .find(|capability| capability.codec == route.codec)
        .filter(|capability| capability.max_packet_ms != 0)
        .ok_or(ServerError::UnsupportedMulticastCodec)?;
    if route.packet_millis > capability.max_packet_ms {
        return Err(ServerError::InvalidMulticastMedia(
            "packet framing exceeds the advertised capability",
        ));
    }
    Ok(())
}

fn allocate_multicast_request_identity(
    state: &mut SessionState,
) -> Result<MediaRequestIdentity, ServerError> {
    let generation = state
        .next_multicast_generation
        .checked_add(1)
        .ok_or(ServerError::MediaRequestIdentityExhausted)?;
    let token = state
        .next_media_token
        .ok_or(ServerError::MediaRequestIdentityExhausted)?;
    let identity = MediaRequestIdentity::new(generation, token)
        .ok_or(ServerError::MediaRequestIdentityExhausted)?;
    state.next_multicast_generation = generation;
    state.next_media_token = token.checked_next();
    Ok(identity)
}

fn multicast_stop_message(
    key: MulticastKey,
    wire_call_reference: u32,
    request: MediaRequestIdentity,
    receive: bool,
) -> ServerMessage {
    if receive {
        ServerMessage::StopMulticastMediaReception {
            conference_id: key.conference_id,
            passthrough_party_id: request.token().get().into(),
            call_reference: CallReference::new(wire_call_reference),
        }
    } else {
        ServerMessage::StopMulticastMediaTransmission {
            conference_id: key.conference_id,
            passthrough_party_id: request.token().get().into(),
            call_reference: CallReference::new(wire_call_reference),
        }
    }
}

fn take_multicast_stop(
    state: &mut SessionState,
    key: MulticastKey,
    receive: bool,
) -> Option<ServerMessage> {
    let session = state.multicast.get_mut(&key)?;
    let request = if receive {
        session.receive.take().map(|leg| leg.request)
    } else {
        session.transmit.take().map(|leg| leg.request)
    }?;
    let message = multicast_stop_message(key, session.wire_call_reference, request, receive);
    if session.receive.is_none() && session.transmit.is_none() {
        state.multicast.remove(&key);
    }
    Some(message)
}

fn expire_multicast_reception_acknowledgements(
    state: &mut SessionState,
    now: Instant,
) -> Vec<(MulticastKey, ServerMessage)> {
    let mut expired = state
        .multicast
        .iter()
        .filter_map(|(key, session)| {
            session.receive.as_ref().and_then(|receive| {
                matches!(
                    receive.state,
                    MulticastReceiveState::AwaitingAcknowledgement { deadline }
                        if deadline <= now
                )
                .then_some(*key)
            })
        })
        .collect::<Vec<_>>();
    expired.sort_unstable_by_key(|key| (key.conference_id.get(), key.call_id.get()));
    expired
        .into_iter()
        .filter_map(|key| take_multicast_stop(state, key, true).map(|stop| (key, stop)))
        .collect()
}

fn take_multicast_stops_for_call(state: &mut SessionState, call_id: CallId) -> Vec<ServerMessage> {
    let mut keys = state
        .multicast
        .keys()
        .copied()
        .filter(|key| key.call_id == call_id)
        .collect::<Vec<_>>();
    keys.sort_unstable_by_key(|key| key.conference_id.get());
    keys.into_iter()
        .flat_map(|key| {
            [
                take_multicast_stop(state, key, true),
                take_multicast_stop(state, key, false),
            ]
            .into_iter()
            .flatten()
        })
        .collect()
}

fn take_all_multicast_stops(state: &mut SessionState) -> Vec<ServerMessage> {
    let mut sessions = std::mem::take(&mut state.multicast)
        .into_iter()
        .collect::<Vec<_>>();
    sessions.sort_unstable_by_key(|(key, _)| (key.conference_id.get(), key.call_id.get()));
    sessions
        .into_iter()
        .flat_map(|(key, session)| {
            [
                session.receive.map(|leg| {
                    multicast_stop_message(key, session.wire_call_reference, leg.request, true)
                }),
                session.transmit.map(|leg| {
                    multicast_stop_message(key, session.wire_call_reference, leg.request, false)
                }),
            ]
            .into_iter()
            .flatten()
        })
        .collect()
}

fn take_all_audio_stops(state: &mut SessionState) -> Vec<ServerMessage> {
    let mut call_ids = state.calls_by_id.keys().copied().collect::<Vec<_>>();
    call_ids.sort_unstable_by_key(|call_id| call_id.get());
    let mut messages = Vec::new();
    for call_id in call_ids {
        let call = state
            .calls_by_id
            .get_mut(&call_id)
            .expect("call identifier came from session state");
        if call.media.transmit.state != MediaChannelState::Closed {
            messages.push(ServerMessage::StopMediaTransmission(AudioStreamControl {
                conference_id: ConferenceId::new(call.wire_reference),
                call_reference: CallReference::new(call.wire_reference),
                passthrough_party_id: media_request_party_id(
                    call.media.transmit.request,
                    call.wire_reference,
                )
                .into(),
                port_handling_flag: 0,
            }));
        }
        if call.media.receive.state != MediaChannelState::Closed {
            messages.push(ServerMessage::CloseReceiveChannel(AudioStreamControl {
                conference_id: ConferenceId::new(call.wire_reference),
                call_reference: CallReference::new(call.wire_reference),
                passthrough_party_id: media_request_party_id(
                    call.media.receive.request,
                    call.wire_reference,
                )
                .into(),
                port_handling_flag: 0,
            }));
        }
        call.media.receive.state = MediaChannelState::Closed;
        call.media.receive.deadline = None;
        call.media.receive.peer = None;
        call.media.transmit.state = MediaChannelState::Closed;
        call.media.transmit.deadline = None;
        call.media.transmit.peer = None;
        call.media.transmit_confirmation = TransmitConfirmation::Inactive;
        call.media.coupled_transmit_endpoint = None;
    }
    messages
}

async fn drain_session_media(
    stream: &mut dyn StationIo,
    state: &mut SessionState,
) -> Result<(), ServerError> {
    let protocol = state.registration.protocol;
    let messages = take_all_audio_stops(state)
        .into_iter()
        .chain(take_all_multimedia_receive_closes(state))
        .chain(take_all_multimedia_transmit_stops(state))
        .chain(take_all_multicast_stops(state));
    let mut first_error = None;
    for message in messages {
        if let Err(error) = send_message(stream, &message, protocol).await
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

async fn stop_call_multicast(
    stream: &mut dyn StationIo,
    state: &mut SessionState,
    call_id: CallId,
    protocol: ProtocolVersion,
) -> Result<(), ServerError> {
    for message in take_multicast_stops_for_call(state, call_id) {
        send_message(stream, &message, protocol).await?;
    }
    Ok(())
}

fn allocate_media_request_identity(
    state: &mut SessionState,
    call_id: CallId,
) -> Result<MediaRequestIdentity, ServerError> {
    let generation = require_call(state, call_id)?
        .media
        .generation
        .checked_add(1)
        .ok_or(ServerError::MediaRequestIdentityExhausted)?;
    let token = state
        .next_media_token
        .ok_or(ServerError::MediaRequestIdentityExhausted)?;
    let identity = MediaRequestIdentity::new(generation, token)
        .ok_or(ServerError::MediaRequestIdentityExhausted)?;
    state.next_media_token = token.checked_next();
    require_call_mut(state, call_id)?.media.generation = generation;
    Ok(identity)
}

fn media_request_party_id(
    request: Option<MediaRequestIdentity>,
    stable_call_reference: u32,
) -> u32 {
    request.map_or(stable_call_reference, |identity| identity.token().get())
}

fn remove_call(state: &mut SessionState, call_id: CallId) {
    if let Some(call) = state.calls_by_id.remove(&call_id) {
        state.calls_by_wire.remove(&call.wire_reference);
        if state.active_call_id == Some(call_id) {
            state.active_call_id = None;
        }
        if state.ringer_owner == Some(call_id) {
            state.ringer_owner = None;
        }
    }
}

fn canonical_ip_address(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(address) => address
            .to_ipv4_mapped()
            .map_or(IpAddr::V6(address), IpAddr::V4),
        address => address,
    }
}

fn server_response_address(
    local: IpAddr,
    configured_ipv4_fallback: Ipv4Addr,
    configured_ipv6_fallback: Option<Ipv6Addr>,
) -> IpAddr {
    match canonical_ip_address(local) {
        IpAddr::V4(address) if address.is_unspecified() => IpAddr::V4(configured_ipv4_fallback),
        IpAddr::V6(address) if address.is_unspecified() => {
            configured_ipv6_fallback.map_or(IpAddr::V4(configured_ipv4_fallback), IpAddr::V6)
        }
        local => local,
    }
}

fn server_response_endpoints(
    context: &SessionContext,
    protocol: ProtocolVersion,
) -> Result<Vec<SignalingServerEndpoint>, ServerError> {
    let local_endpoint = || {
        let address = server_response_address(
            context.local.ip(),
            context.config.advertised_address,
            context.config.advertised_ipv6_address,
        );
        let address = if protocol < ProtocolVersion::V17 && address.is_ipv6() {
            IpAddr::V4(context.config.advertised_address)
        } else {
            address
        };
        if address.is_unspecified() {
            return Err(ServerError::InvalidConfig(
                "server-list fallback address is unspecified".into(),
            ));
        }
        Ok(SignalingServerEndpoint {
            name: context.config.server_name.clone(),
            address,
            port: NonZeroU16::new(context.local.port()).ok_or_else(|| {
                ServerError::InvalidConfig("accepted local endpoint has port zero".into())
            })?,
        })
    };
    if context.config.signaling_servers.is_empty() {
        return local_endpoint().map(|endpoint| vec![endpoint]);
    }

    let mut routes = context.config.signaling_servers.iter().collect::<Vec<_>>();
    routes.sort_unstable_by_key(|route| route.priority);
    let endpoints = routes
        .into_iter()
        .filter(|route| protocol >= ProtocolVersion::V17 || route.address.is_ipv4())
        .filter_map(|route| route.endpoint(context.transport))
        .collect::<Vec<_>>();
    if endpoints.is_empty() {
        local_endpoint().map(|endpoint| vec![endpoint])
    } else {
        Ok(endpoints)
    }
}

async fn close_call_media_messages(
    stream: &mut dyn StationIo,
    call: &SessionCall,
    protocol: ProtocolVersion,
) -> Result<(), ServerError> {
    if let Some(leg) = &call.video_receive.leg {
        send_message(
            stream,
            &multimedia_receive_close_message(call, leg),
            protocol,
        )
        .await?;
    }
    if let Some(leg) = &call.video_transmit.leg {
        send_message(
            stream,
            &multimedia_transmit_stop_message(call, leg),
            protocol,
        )
        .await?;
    }
    if call.media.receive.state != MediaChannelState::Closed {
        send_message(
            stream,
            &ServerMessage::CloseReceiveChannel(AudioStreamControl {
                conference_id: ConferenceId::new(call.wire_reference),
                call_reference: CallReference::new(call.wire_reference),
                passthrough_party_id: media_request_party_id(
                    call.media.receive.request,
                    call.wire_reference,
                )
                .into(),
                port_handling_flag: 0,
            }),
            protocol,
        )
        .await?;
    }
    if call.media.transmit.state != MediaChannelState::Closed {
        send_message(
            stream,
            &ServerMessage::StopMediaTransmission(AudioStreamControl {
                conference_id: ConferenceId::new(call.wire_reference),
                call_reference: CallReference::new(call.wire_reference),
                passthrough_party_id: media_request_party_id(
                    call.media.transmit.request,
                    call.wire_reference,
                )
                .into(),
                port_handling_flag: 0,
            }),
            protocol,
        )
        .await?;
    }
    Ok(())
}

async fn close_call_messages(
    stream: &mut dyn StationIo,
    call: &SessionCall,
    soft_keys: &SoftKeyProfile,
    protocol: ProtocolVersion,
    timezone_offset_minutes: i16,
    stop_ringer: bool,
) -> Result<(), ServerError> {
    send_message(
        stream,
        &ServerMessage::StopTone {
            line_instance: call.line_instance,
            call_reference: call.wire_reference,
        },
        protocol,
    )
    .await?;
    send_message(
        stream,
        &ServerMessage::SetLamp {
            stimulus: ButtonType::Line,
            instance: call.line_instance,
            mode: LampMode::Off,
        },
        protocol,
    )
    .await?;
    send_message(
        stream,
        &ServerMessage::ClearPrompt {
            line_instance: call.line_instance,
            call_reference: call.wire_reference,
        },
        protocol,
    )
    .await?;
    send_message(
        stream,
        &ServerMessage::CallState {
            state: CallState::OnHook,
            line_instance: call.line_instance,
            call_reference: call.wire_reference,
        },
        protocol,
    )
    .await?;
    send_message(
        stream,
        &ServerMessage::SelectSoftKeys {
            line_instance: 0,
            call_reference: 0,
            set: KeyMode::OnHook,
            valid_mask: soft_keys.valid_mask(KeyMode::OnHook),
        },
        protocol,
    )
    .await?;
    send_message(
        stream,
        &time_date_message(timezone_offset_minutes),
        protocol,
    )
    .await?;
    send_message(
        stream,
        &ServerMessage::SetSpeakerMode(SpeakerMode::Off),
        protocol,
    )
    .await?;
    if stop_ringer {
        send_message(
            stream,
            &ServerMessage::SetRinger {
                mode: RingerMode::Off,
                duration: RingDuration::Normal,
                line_instance: call.line_instance,
                call_reference: call.wire_reference,
            },
            protocol,
        )
        .await?;
    }
    Ok(())
}

fn time_date_message(timezone_offset_minutes: i16) -> ServerMessage {
    time_date_message_at(SystemTime::now(), timezone_offset_minutes)
}

fn time_date_message_at(now: SystemTime, timezone_offset_minutes: i16) -> ServerMessage {
    let unix = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let local = (unix as i128 + i128::from(timezone_offset_minutes) * 60)
        .clamp(0, i128::from(u32::MAX)) as u64;
    let days = (local / 86_400) as i64;
    let seconds = local % 86_400;
    let (year, month, day) = civil_from_days(days);
    ServerMessage::TimeDate {
        year: year as u32,
        month,
        weekday: ((days + 4).rem_euclid(7) + 1) as u32,
        day,
        hour: (seconds / 3600) as u32,
        minute: ((seconds % 3600) / 60) as u32,
        second: (seconds % 60) as u32,
        milliseconds: 0,
        unix_seconds: local as u32,
    }
}

// Howard Hinnant's civil-from-days algorithm, with days based at Unix epoch.
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month as u32, day as u32)
}

#[cfg(test)]
#[path = "server/tests/mod.rs"]
mod tests;
