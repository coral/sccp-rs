//! Typed SCCP messages used by the server.
//!
//! This module also exposes wire values, framing, and contract metadata.
//!
//! A typical inbound flow feeds TCP bytes to [`wire::FrameDecoder`], validates
//! the negotiated [`values::ProtocolVersion`], then decodes the frame as
//! [`ClientMessage`], [`ServerMessage`], or [`ControlMessage`] according to its
//! [`catalog::MessageRoute`]. Outbound typed messages expose `encode` methods
//! implemented by the private codec module. Unknown identifiers and partially
//! modeled fields have explicit bounded-preservation types rather than being
//! silently discarded.

mod bounded;
pub mod capabilities;
pub mod catalog;
mod codec;
pub mod values;
pub mod wire;

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::num::{NonZeroU16, NonZeroU32};

use crate::types::DateTemplate;
use crate::types::{
    ApplicationId, CallInfo, CallReference, ConferenceId, DeviceId, MediaEndpoint, SoftKeyProfile,
    TransactionId,
};
use capabilities::CapabilityUpdate;
use catalog::MessageId;
pub(crate) use catalog::wire_id;
use values::{
    AddParticipantResult, AlarmSeverity, AnnouncementPlayMode, AnnouncementPlayStatus,
    AuditParticipantResult, BusyLampFieldState, ButtonType, CallHistoryDisposition, CallState,
    Codec, ConferenceResourceType, CreateConferenceResult, DeleteConferenceResult, DeviceType,
    Digit, EchoCancellation, EncryptionMethod, EndOfAnnouncementAck, G723BitRate, IpAddressType,
    KeyMode, LampMode, MediaPathCapability, MediaPathEvent, MediaPathId, MediaStatus,
    MediaTransport, MediaType, MessageWaitingResult, MicrophoneMode, ModifyConferenceResult,
    NotificationPriority, PartyInformationRestrictions, PhoneFeatures, ProtocolVersion,
    QosDirection, QosErrorCode, QosReservationStyle, ResetType, RingDuration, RingerMode,
    RsvpErrorCode, SilenceSuppression, SpeakerMode, StatisticsProcessing, Stimulus,
    SubscriptionCause, Tone, ToneDirection, VideoFormat,
};
use wire::CodecError;

pub use bounded::{BoundedBytes, BoundedBytesError};

/// Largest opaque body retained from a valid frame.
pub const MAX_OPAQUE_MESSAGE_BYTES: usize = wire::MAX_FRAME_SIZE - wire::HEADER_SIZE;

/// Width of the codec-specific capability union in multimedia channel messages.
pub const MULTIMEDIA_CAPABILITY_BYTES: usize = 76;
/// Maximum picture-format entries in one multimedia video capability.
pub const MAX_MULTIMEDIA_PICTURE_FORMATS: usize = 5;

/// Number of definitions reserved by the fixed 96-byte ButtonTemplate body.
pub(crate) const BUTTON_TEMPLATE_ENTRIES_PER_CHUNK: usize = 42;

/// Non-zero token placed in the SCCP pass-through-party field to identify one
/// media request generation, rather than the lifetime of a call.
///
/// Phones echo this field on conforming ORC/SMT acknowledgements. Changing it
/// per request prevents a delayed ACK for a retired request from matching a
/// later reopen and supplies explicit wire correlation to both ACK families.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MediaRequestToken(NonZeroU32);

impl MediaRequestToken {
    /// Creates a token, returning `None` for the reserved value zero.
    pub const fn new(value: u32) -> Option<Self> {
        match NonZeroU32::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    pub const fn get(self) -> u32 {
        self.0.get()
    }

    /// Advance without wrapping or reusing token zero.
    ///
    /// Exhaustion is an explicit failure: silently wrapping would make an
    /// ancient acknowledgement eligible to match a new request.
    pub const fn checked_next(self) -> Option<Self> {
        match self.get().checked_add(1) {
            Some(value) => Self::new(value),
            None => None,
        }
    }
}

/// Pending identity used to decide whether a handset media ACK belongs to the
/// currently opening receive/transmit request.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct MediaRequestIdentity {
    generation: u64,
    token: MediaRequestToken,
}

impl MediaRequestIdentity {
    /// Construct an identity. Generations are monotonic per call and start at
    /// one; a deliberately coupled ORC/SMT pair shares one identity. Tokens
    /// must be allocated uniquely among live and retired media sessions.
    pub const fn new(generation: u64, token: MediaRequestToken) -> Option<Self> {
        if generation == 0 {
            None
        } else {
            Some(Self { generation, token })
        }
    }

    pub const fn generation(self) -> u64 {
        self.generation
    }

    pub const fn token(self) -> MediaRequestToken {
        self.token
    }

    /// Advance both the logical generation and its wire token without wrap.
    /// A caller must fail the media reopen when this returns `None`.
    pub const fn checked_next(self) -> Option<Self> {
        let generation = match self.generation.checked_add(1) {
            Some(generation) => generation,
            None => return None,
        };
        let token = match self.token.checked_next() {
            Some(token) => token,
            None => return None,
        };
        Some(Self { generation, token })
    }

    /// Match an ACK without permitting a prior generation to settle a reopen.
    ///
    /// An SMT acknowledgement may omit the party ID. That fallback is safe
    /// only for generation one and only with the stable call reference;
    /// after a reopen, a zero-party ACK is intrinsically ambiguous and fails
    /// closed. A present party ID must match the fresh token, while a present
    /// call reference must still identify the same call.
    pub const fn accepts_ack(
        self,
        acknowledgement_party_id: u32,
        acknowledgement_call_reference: u32,
        stable_call_reference: u32,
    ) -> bool {
        let call_matches = acknowledgement_call_reference == 0
            || acknowledgement_call_reference == stable_call_reference;
        if acknowledgement_party_id == self.token.get() {
            return call_matches;
        }
        self.generation == 1
            && acknowledgement_party_id == 0
            && acknowledgement_call_reference == stable_call_reference
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// An unrecognized frame retained without interpreting its identifier or payload.
pub struct RawMessage {
    pub message_id: u32,
    pub protocol_version: u32,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Station registration identity, addressing, capacity, and feature data.
///
/// The codec accepts both mandatory and extended registration bodies. Extended
/// capacity fields are available through [`RegistrationMessage::wire`].
pub struct RegistrationMessage {
    pub device_id: DeviceId,
    /// IPv4 address claimed by the station, independent of its TCP peer address.
    pub reported_address: Option<Ipv4Addr>,
    /// IPv6 address claimed by the station when the extended layout carries one.
    pub reported_ipv6_address: Option<Ipv6Addr>,
    pub device_type: DeviceType,
    /// Raw protocol version advertised inside the registration body.
    /// Session code must validate/negotiate this through [`ProtocolVersion`].
    pub advertised_protocol: Option<u32>,
    /// Feature bits packed alongside the advertised body version.
    pub features: PhoneFeatures,
    pub firmware: String,
    /// Bytes following the mandatory registration prefix.
    pub configuration_version_stamp: BoundedBytes<48>,
    /// Exact capacity and addressing metadata from the extended registration
    /// layout. Runtime-created registrations may omit it and receive the
    /// conservative wire defaults used by the encoder.
    pub wire: Option<RegistrationWireDetails>,
}

/// Auxiliary fields carried by the extended station registration layout.
///
/// These fields are not registration policy, but retaining them prevents a
/// decode/encode cycle from erasing capacity, scope, or station identity data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegistrationWireDetails {
    pub layout: RegistrationWireLayout,
    pub station_user_id: u32,
    pub station_instance: u32,
    pub max_streams: u32,
    pub active_streams: u32,
    /// Six MAC bytes followed by the six documented reserved bytes.
    pub mac_address_and_padding: [u8; 12],
    pub max_conferences: u32,
    pub active_conferences: u32,
    /// Address-scope word associated with the reported IPv4 address.
    pub ipv4_address_scope: u32,
    pub max_lines: u32,
    /// Address-scope word associated with the reported IPv6 address.
    pub ipv6_address_scope: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistrationWireLayout {
    Alternate32,
    Canonical { prefix_bytes: u8 },
}

impl Default for RegistrationWireLayout {
    fn default() -> Self {
        Self::Canonical { prefix_bytes: 124 }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// One audio codec capability advertised by a station.
pub struct MediaCapability {
    pub codec: Codec,
    pub max_packet_ms: u32,
    /// Fixed codec-specific parameter area retained byte-for-byte.
    pub codec_parameters: [u8; 8],
}

/// Width of the extended opaque call-count request body.
pub const CALL_COUNT_REQUEST_EXTENDED_BYTES: usize = 152;
/// Number of line records reserved by the fixed call-count response body.
pub const CALL_COUNT_RESPONSE_MAX_LINE_ENTRIES: usize = 42;

/// Known wire dialects of the otherwise fieldless call-count request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CallCountRequestPayload {
    /// Fieldless request emitted by Cisco 8945 firmware.
    Empty,
    /// One unknown word declared by legacy chan-sccp sources.
    LegacyWord(u32),
    /// Extended opaque request body used by one protocol dialect.
    Extended([u8; CALL_COUNT_REQUEST_EXTENDED_BYTES]),
}

/// Per-line capacity advertised in a call-count response.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CallCountLineData {
    pub max_calls: u16,
    pub busy_trigger: u16,
}

/// Fixed-array response describing the configured capacity of station lines.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CallCountResponse {
    pub total_configured_lines: u32,
    pub starting_line_instance: u32,
    /// Active records; the wire body reserves 42 entries and zero-fills the rest.
    pub line_data: Vec<CallCountLineData>,
}

pub const MEDIA_PORT_LIST_MAX_PORTS: usize = 16;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaPortList {
    pub rtp_ports: Vec<u16>,
}

/// SRTP keying material. Debug output intentionally exposes metadata only.
#[derive(Clone, Eq, PartialEq)]
pub struct MediaEncryption {
    pub algorithm: EncryptionMethod,
    key: [u8; 16],
    key_length: u8,
    salt: [u8; 16],
    salt_length: u8,
    /// Non-zero when the media packet carries a master-key identifier.
    pub mki_present: u32,
    /// SRTP key-derivation rate word.
    pub key_derivation_rate: u32,
}

impl MediaEncryption {
    /// Copies validated SRTP keying material into redacted, zeroizing storage.
    ///
    /// Keys and salts are independently limited to 16 bytes.
    pub fn new(
        algorithm: EncryptionMethod,
        key: &[u8],
        salt: &[u8],
        mki_present: u32,
        key_derivation_rate: u32,
    ) -> Result<Self, CodecError> {
        if key.len() > 16 {
            return Err(CodecError::SecretTooLong {
                field: "media encryption key",
                actual: key.len(),
                maximum: 16,
            });
        }
        if salt.len() > 16 {
            return Err(CodecError::SecretTooLong {
                field: "media encryption salt",
                actual: salt.len(),
                maximum: 16,
            });
        }
        let mut wire_key = [0; 16];
        wire_key[..key.len()].copy_from_slice(key);
        let mut wire_salt = [0; 16];
        wire_salt[..salt.len()].copy_from_slice(salt);
        Ok(Self {
            algorithm,
            key: wire_key,
            key_length: key.len() as u8,
            salt: wire_salt,
            salt_length: salt.len() as u8,
            mki_present,
            key_derivation_rate,
        })
    }

    pub(crate) const fn from_wire(
        algorithm: EncryptionMethod,
        key: [u8; 16],
        key_length: u8,
        salt: [u8; 16],
        salt_length: u8,
        mki_present: u32,
        key_derivation_rate: u32,
    ) -> Self {
        Self {
            algorithm,
            key,
            key_length,
            salt,
            salt_length,
            mki_present,
            key_derivation_rate,
        }
    }

    pub fn key(&self) -> &[u8] {
        &self.key[..usize::from(self.key_length)]
    }

    pub fn salt(&self) -> &[u8] {
        &self.salt[..usize::from(self.salt_length)]
    }
}

impl fmt::Debug for MediaEncryption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MediaEncryption")
            .field("algorithm", &self.algorithm)
            .field("key", &"<redacted>")
            .field("key_len", &self.key_length)
            .field("salt", &"<redacted>")
            .field("salt_len", &self.salt_length)
            .field("mki_present", &self.mki_present)
            .field("key_derivation_rate", &self.key_derivation_rate)
            .finish()
    }
}

impl Drop for MediaEncryption {
    fn drop(&mut self) {
        self.key.fill(0);
        self.salt.fill(0);
    }
}

/// One locale-aware tone in a station announcement sequence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AnnouncementEntry {
    pub locale: u32,
    pub country: u32,
    pub tone: Tone,
}

/// Parameters and application data for creating a station-managed conference.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateConferenceRequest {
    pub conference_id: ConferenceId,
    pub reserved_participants: u32,
    pub resource_type: ConferenceResourceType,
    pub application_id: ApplicationId,
    pub application_conference_id: String,
    pub application_data: String,
    pub passthrough_data: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Result and returned application bytes for conference creation.
pub struct CreateConferenceResponse {
    pub conference_id: ConferenceId,
    pub result: CreateConferenceResult,
    pub passthrough_data: Vec<u8>,
}

/// Parameters and application data for resizing or updating a conference.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModifyConferenceRequest {
    pub conference_id: ConferenceId,
    pub reserved_participants: u32,
    pub application_id: ApplicationId,
    pub application_conference_id: String,
    pub application_data: String,
    pub passthrough_data: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Result and returned application bytes for conference modification.
pub struct ModifyConferenceResponse {
    pub conference_id: ConferenceId,
    pub result: ModifyConferenceResult,
    pub passthrough_data: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// One conference record returned by an audit operation.
pub struct AuditConferenceEntry {
    pub conference_id: ConferenceId,
    pub resource_type: ConferenceResourceType,
    pub reserved_participants: u32,
    pub active_participants: u32,
    pub application_id: ApplicationId,
    pub application_conference_id: String,
    pub application_data: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// A page of conference audit records.
pub struct AuditConferenceResponse {
    /// Non-zero when this page is the final audit response.
    pub last: u32,
    pub entries: Vec<AuditConferenceEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Presentation identity and call reference for a conference participant.
pub struct ConferenceParticipant {
    pub call_reference: CallReference,
    pub presentation_restrictions: PartyInformationRestrictions,
    pub name: String,
    pub number: String,
    pub conference_name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Request to attach a call participant to a conference.
pub struct AddParticipantRequest {
    pub conference_id: ConferenceId,
    pub participant: ConferenceParticipant,
}

/// Update the presentation identity of an existing conference participant.
///
/// This is the standalone intra-control `0x013e` request. It intentionally
/// shares the participant layout with [`AddParticipantRequest`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeParticipantRequest {
    pub conference_id: ConferenceId,
    pub participant: ConferenceParticipant,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Result of adding a participant, including the service-assigned identity.
pub struct AddParticipantResponse {
    pub conference_id: ConferenceId,
    pub call_reference: CallReference,
    pub result: AddParticipantResult,
    /// Opaque service-assigned participant identity, bounded to its wire field.
    pub bridge_participant_id: BoundedBytes<257>,
}

/// Participant audit entry bytes have an opaque schema. The typed envelope
/// preserves them losslessly while enforcing the aggregate wire bound.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditParticipantResponse {
    pub result: AuditParticipantResult,
    pub last: u32,
    pub conference_id: ConferenceId,
    /// Declared entry count retained separately from the opaque entry bytes.
    pub number_of_entries: u32,
    /// Opaque participant records retained in their received order.
    pub participant_entries: Vec<u8>,
}

/// Routing metadata for a participant change carried by the V1 application
/// envelope rather than a standalone station message identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParticipantChangeRouting {
    pub application_id: ApplicationId,
    pub line_instance: u32,
    pub transaction_id: TransactionId,
    pub sequence_flag: u32,
    pub display_priority: u32,
    pub application_instance_id: ApplicationId,
    pub routing: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// A participant-identity change independent of application-envelope routing.
pub struct ConferenceParticipantChange {
    pub conference_id: ConferenceId,
    pub participant: ConferenceParticipant,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Parameters for receiving an audio stream from a multicast endpoint.
pub struct MulticastMediaReception {
    pub conference_id: ConferenceId,
    pub passthrough_party_id: crate::types::PassthroughPartyId,
    pub call_reference: CallReference,
    pub address: IpAddr,
    pub port: u16,
    pub packet_millis: u32,
    pub codec: Codec,
    pub echo_cancellation: EchoCancellation,
    pub g723_bitrate: G723BitRate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Parameters for transmitting an audio stream to a multicast endpoint.
pub struct MulticastMediaTransmission {
    pub conference_id: ConferenceId,
    pub passthrough_party_id: crate::types::PassthroughPartyId,
    pub call_reference: CallReference,
    pub address: IpAddr,
    pub port: u16,
    pub packet_millis: u32,
    pub codec: Codec,
    pub precedence: u32,
    pub silence_suppression: u32,
    pub max_frames_per_packet: u32,
    pub g723_bitrate: G723BitRate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// A cataloged but untyped message retained for explicit bounded forwarding.
pub struct KnownOpaqueMessage {
    pub id: MessageId,
    pub protocol_version: u32,
    pub payload: BoundedBytes<MAX_OPAQUE_MESSAGE_BYTES>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Original application-data envelope with routing identifiers and opaque data.
pub struct UserDataMessage {
    pub application_id: u32,
    pub line_instance: u32,
    pub call_reference: u32,
    pub transaction_id: u32,
    pub data: Vec<u8>,
}

/// The extended XML/application-data envelope introduced after SCCP v3.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserDataV1Message {
    pub application_id: u32,
    pub line_instance: u32,
    pub call_reference: u32,
    pub transaction_id: u32,
    pub sequence_flag: u32,
    pub display_priority: u32,
    pub conference_id: u32,
    pub application_instance_id: u32,
    pub routing: u32,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Station token-registration identity and network endpoint.
pub struct RegisterTokenMessage {
    pub device_id: DeviceId,
    pub device_instance: u32,
    pub address: IpAddr,
    pub device_type: DeviceType,
    /// Firmware flags whose meaning is not fully documented.
    pub flags: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpcpRegisterTokenMessage {
    pub device_id: DeviceId,
    pub device_instance: u32,
    pub address: Ipv4Addr,
    pub device_type: DeviceType,
    pub max_streams: u32,
}

/// Maximum endpoints carried by one station server-list response.
pub const MAX_SIGNALING_SERVERS: usize = 5;

/// One reachable control endpoint in a station server-list response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SignalingServerEndpoint {
    pub name: String,
    pub address: IpAddr,
    pub port: NonZeroU16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Media-resource service capacity notification.
pub struct MediaResourceNotification {
    pub device_type: DeviceType,
    pub in_service_streams: u32,
    pub max_streams_per_conference: u32,
    pub out_of_service_streams: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Request to create or renew a feature subscription.
pub struct SubscriptionRequest {
    pub transaction_id: u32,
    pub feature_id: u32,
    pub timer_seconds: u32,
    pub subscription_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Allocated RTP/RTCP endpoint returned for a media flow.
pub struct PortEndpoint {
    pub conference_id: u32,
    pub call_reference: u32,
    pub passthrough_party_id: u32,
    pub address: IpAddr,
    pub rtp_port: u16,
    pub rtcp_port: u16,
    pub media_type: Option<MediaType>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Request to allocate an endpoint for one media flow.
pub struct PortRequest {
    pub conference_id: ConferenceId,
    pub call_reference: CallReference,
    pub passthrough_party_id: crate::types::PassthroughPartyId,
    pub transport: MediaTransport,
    pub address_type: Option<IpAddressType>,
    pub media_type: Option<MediaType>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Request to release a previously allocated media endpoint.
pub struct PortClose {
    pub conference_id: ConferenceId,
    pub call_reference: CallReference,
    pub passthrough_party_id: crate::types::PassthroughPartyId,
    pub media_type: Option<MediaType>,
}

/// Addressed media flow used by the intra-control QoS message family.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct QosFlow {
    pub conference_id: ConferenceId,
    pub call_reference: CallReference,
    pub passthrough_party_id: crate::types::PassthroughPartyId,
    pub address: Ipv4Addr,
    pub port: u16,
}

/// RSVP traffic parameters.
///
/// The codec identifier remains forward-compatible through [`Codec::Unknown`];
/// the rate and burst values are protocol quantities rather than closed enums.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QosTrafficSpecification {
    pub codec: Codec,
    pub average_bit_rate: u32,
    pub burst_size: u32,
    pub peak_rate: u32,
}

/// Fixed application identity carried by QoS listen/path/modify requests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QosApplicationIdentifier {
    pub vendor_id: String,
    pub version: String,
    pub application_name: String,
    pub sub_application_id: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
/// New and previously heard message counts for one mailbox category.
pub struct MessageWaitingCounts {
    pub new: u32,
    pub old: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Message-waiting state and category counts for one target number.
pub struct MessageWaitingNotification {
    pub target_number: String,
    pub control_number: String,
    pub messages_waiting: bool,
    pub total_voicemail: MessageWaitingCounts,
    pub priority_voicemail: MessageWaitingCounts,
    pub total_fax: MessageWaitingCounts,
    pub priority_fax: MessageWaitingCounts,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Station acknowledgement for an opened multimedia receive channel.
pub struct OpenMultimediaReceiveChannelAck {
    pub status: MediaStatus,
    pub endpoint: MediaEndpointAddress,
    pub passthrough_party_id: crate::types::PassthroughPartyId,
    pub call_reference: CallReference,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Station acknowledgement for a multimedia transmit request.
pub struct StartMultimediaTransmissionAck {
    pub conference_id: ConferenceId,
    pub passthrough_party_id: crate::types::PassthroughPartyId,
    pub call_reference: CallReference,
    pub endpoint: MediaEndpointAddress,
    pub status: MediaStatus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Network address and transport port for a media endpoint.
pub struct MediaEndpointAddress {
    pub address: IpAddr,
    pub port: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Identity fields shared by multimedia close and stop commands.
pub struct MultimediaStreamControl {
    pub conference_id: ConferenceId,
    pub passthrough_party_id: crate::types::PassthroughPartyId,
    pub call_reference: CallReference,
    pub port_handling_flag: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Identity fields shared by audio receive-close and transmit-stop commands.
pub struct AudioStreamControl {
    pub conference_id: ConferenceId,
    pub passthrough_party_id: crate::types::PassthroughPartyId,
    pub call_reference: CallReference,
    pub port_handling_flag: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Remote address and type for starting or stopping a control session.
pub struct SessionTransmission {
    pub remote_address: IpAddr,
    pub session_type: u32,
}

/// Seven-bit RTP payload number used by a multimedia stream.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RtpPayloadNumber(u8);

impl RtpPayloadNumber {
    pub const MAX: u32 = 127;

    pub const fn new(value: u32) -> Result<Self, RtpPayloadNumberError> {
        if value <= Self::MAX {
            Ok(Self(value as u8))
        } else {
            Err(RtpPayloadNumberError { actual: value })
        }
    }

    pub const fn get(self) -> u8 {
        self.0
    }
}

impl TryFrom<u32> for RtpPayloadNumber {
    type Error = RtpPayloadNumberError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<RtpPayloadNumber> for u32 {
    fn from(value: RtpPayloadNumber) -> Self {
        u32::from(value.get())
    }
}

/// Failure returned when a value is outside the RTP payload-number range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RtpPayloadNumberError {
    pub actual: u32,
}

impl fmt::Display for RtpPayloadNumberError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "RTP payload number {} exceeds {}",
            self.actual,
            RtpPayloadNumber::MAX
        )
    }
}

impl std::error::Error for RtpPayloadNumberError {}

/// Two-word multimedia RTP descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MultimediaPayloadDescriptor {
    rfc_number: u32,
    payload_number: RtpPayloadNumber,
}

impl MultimediaPayloadDescriptor {
    /// Retains the packetization-format flags independently from the RTP payload number.
    pub const fn new(rfc_number: u32, payload_number: RtpPayloadNumber) -> Self {
        Self {
            rfc_number,
            payload_number,
        }
    }

    /// Returns the preserved first descriptor word.
    pub const fn rfc_number(self) -> u32 {
        self.rfc_number
    }

    pub const fn payload_number(self) -> RtpPayloadNumber {
        self.payload_number
    }
}

/// One supported picture format and its minimum picture interval.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MultimediaPictureFormat {
    pub format: VideoFormat,
    pub minimum_picture_interval: u32,
}

/// Codec-selected arm of a multimedia video capability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MultimediaVideoCapabilityArm {
    H261 {
        temporal_spatial_trade_off_capability: u32,
        still_image_transmission: u32,
    },
    H263 {
        capability_bitfield: u32,
        annex_n_and_w_future_use: u32,
    },
    H263Plus {
        model_number: u32,
        bandwidth: u32,
    },
    H264 {
        profile: u32,
        level: u32,
        custom_max_mbps: u32,
        custom_max_fs: u32,
        custom_max_dpb: u32,
        custom_max_br_and_cpb: u32,
    },
}

impl MultimediaVideoCapabilityArm {
    pub const fn codec(self) -> Codec {
        match self {
            Self::H261 { .. } => Codec::H261,
            Self::H263 { .. } => Codec::H263,
            Self::H263Plus { .. } => Codec::H263Plus,
            Self::H264 { .. } => Codec::H264,
        }
    }
}

/// Fully modeled video arm of a multimedia channel command.
#[derive(Clone)]
pub struct MultimediaVideoCapability {
    bit_rate: u32,
    picture_formats: Box<[MultimediaPictureFormat]>,
    conference_service_number: u32,
    arm: MultimediaVideoCapabilityArm,
    preserved_wire: Option<[u8; MULTIMEDIA_CAPABILITY_BYTES]>,
}

impl MultimediaVideoCapability {
    /// Builds a video capability when its picture-format list fits the wire table.
    pub fn new(
        bit_rate: u32,
        picture_formats: impl IntoIterator<Item = MultimediaPictureFormat>,
        conference_service_number: u32,
        arm: MultimediaVideoCapabilityArm,
    ) -> Result<Self, MultimediaCapabilityError> {
        let picture_formats = picture_formats.into_iter().collect::<Box<[_]>>();
        if picture_formats.len() > MAX_MULTIMEDIA_PICTURE_FORMATS {
            return Err(MultimediaCapabilityError {
                maximum: MAX_MULTIMEDIA_PICTURE_FORMATS,
                actual: picture_formats.len(),
            });
        }
        Ok(Self {
            bit_rate,
            picture_formats,
            conference_service_number,
            arm,
            preserved_wire: None,
        })
    }

    pub const fn bit_rate(&self) -> u32 {
        self.bit_rate
    }

    pub fn picture_formats(&self) -> &[MultimediaPictureFormat] {
        &self.picture_formats
    }

    pub const fn conference_service_number(&self) -> u32 {
        self.conference_service_number
    }

    pub const fn arm(&self) -> MultimediaVideoCapabilityArm {
        self.arm
    }

    pub const fn codec(&self) -> Codec {
        self.arm.codec()
    }
}

impl fmt::Debug for MultimediaVideoCapability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MultimediaVideoCapability")
            .field("bit_rate", &self.bit_rate)
            .field("picture_formats", &self.picture_formats)
            .field("conference_service_number", &self.conference_service_number)
            .field("arm", &self.arm)
            .finish()
    }
}

impl PartialEq for MultimediaVideoCapability {
    fn eq(&self, other: &Self) -> bool {
        self.bit_rate == other.bit_rate
            && self.picture_formats == other.picture_formats
            && self.conference_service_number == other.conference_service_number
            && self.arm == other.arm
            && self.preserved_wire == other.preserved_wire
    }
}

impl Eq for MultimediaVideoCapability {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Failure returned when a video capability exceeds a fixed table bound.
pub struct MultimediaCapabilityError {
    pub maximum: usize,
    pub actual: usize,
}

impl fmt::Display for MultimediaCapabilityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "video capability contains {} picture formats, exceeding the maximum of {}",
            self.actual, self.maximum
        )
    }
}

impl std::error::Error for MultimediaCapabilityError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MultimediaPayloadDirection {
    Receive,
    Transmit,
}

#[derive(Clone, Eq, PartialEq)]
enum MultimediaCapabilityState {
    Video(MultimediaVideoCapability),
    Preserved([u8; MULTIMEDIA_CAPABILITY_BYTES]),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MultimediaPayloadOrigin {
    Constructed,
    Decoded {
        direction: MultimediaPayloadDirection,
        protocol: ProtocolVersion,
        compression_codec: Codec,
    },
}

/// RTP descriptor and codec-selected capability for a multimedia stream.
#[derive(Clone)]
pub struct MultimediaPayload {
    descriptor: MultimediaPayloadDescriptor,
    capability: MultimediaCapabilityState,
    origin: MultimediaPayloadOrigin,
}

impl MultimediaPayload {
    /// Constructs an outbound payload using the capability arm as its codec selector.
    pub fn new(payload_number: RtpPayloadNumber, capability: MultimediaVideoCapability) -> Self {
        Self::with_descriptor(
            MultimediaPayloadDescriptor::new(0, payload_number),
            capability,
        )
    }

    /// Constructs a payload with explicit packetization-format flags.
    pub fn with_descriptor(
        descriptor: MultimediaPayloadDescriptor,
        capability: MultimediaVideoCapability,
    ) -> Self {
        Self {
            descriptor,
            capability: MultimediaCapabilityState::Video(capability),
            origin: MultimediaPayloadOrigin::Constructed,
        }
    }

    const fn from_decoded(
        descriptor: MultimediaPayloadDescriptor,
        capability: MultimediaCapabilityState,
        direction: MultimediaPayloadDirection,
        protocol: ProtocolVersion,
        compression_codec: Codec,
    ) -> Self {
        Self {
            descriptor,
            capability,
            origin: MultimediaPayloadOrigin::Decoded {
                direction,
                protocol,
                compression_codec,
            },
        }
    }

    #[cfg(test)]
    pub(crate) fn from_wire(
        rfc_number: u32,
        payload_number: RtpPayloadNumber,
        capability: [u8; MULTIMEDIA_CAPABILITY_BYTES],
        codec: Codec,
        direction: MultimediaPayloadDirection,
        protocol: ProtocolVersion,
    ) -> Self {
        Self::from_decoded(
            MultimediaPayloadDescriptor::new(rfc_number, payload_number),
            MultimediaCapabilityState::Preserved(capability),
            direction,
            protocol,
            codec,
        )
    }

    pub const fn descriptor(&self) -> MultimediaPayloadDescriptor {
        self.descriptor
    }

    pub const fn codec(&self) -> Codec {
        self.compression_codec()
    }

    pub const fn payload_number(&self) -> RtpPayloadNumber {
        self.descriptor.payload_number()
    }

    /// Returns `None` for a decoded codec arm without a structured model.
    pub const fn video_capability(&self) -> Option<&MultimediaVideoCapability> {
        match &self.capability {
            MultimediaCapabilityState::Video(capability) => Some(capability),
            MultimediaCapabilityState::Preserved(_) => None,
        }
    }

    pub(crate) fn is_valid_for(
        &self,
        direction: MultimediaPayloadDirection,
        protocol: ProtocolVersion,
    ) -> bool {
        match self.origin {
            MultimediaPayloadOrigin::Constructed => true,
            MultimediaPayloadOrigin::Decoded {
                direction: decoded_direction,
                protocol: decoded_protocol,
                ..
            } => decoded_direction == direction && decoded_protocol.wire() == protocol.wire(),
        }
    }

    pub(crate) fn is_direction(&self, direction: MultimediaPayloadDirection) -> bool {
        match self.origin {
            MultimediaPayloadOrigin::Constructed => true,
            MultimediaPayloadOrigin::Decoded {
                direction: decoded_direction,
                ..
            } => decoded_direction == direction,
        }
    }

    pub(crate) const fn compression_codec(&self) -> Codec {
        match self.origin {
            MultimediaPayloadOrigin::Constructed => match &self.capability {
                MultimediaCapabilityState::Video(capability) => capability.codec(),
                MultimediaCapabilityState::Preserved(_) => unreachable!(),
            },
            MultimediaPayloadOrigin::Decoded {
                compression_codec, ..
            } => compression_codec,
        }
    }
}

impl PartialEq for MultimediaPayload {
    fn eq(&self, other: &Self) -> bool {
        self.descriptor == other.descriptor
            && self.capability == other.capability
            && self.origin == other.origin
    }
}

impl Eq for MultimediaPayload {}

impl fmt::Debug for MultimediaPayload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MultimediaPayload")
            .field("descriptor", &self.descriptor)
            .field("codec", &self.codec())
            .field("video_capability", &self.video_capability())
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Request to open a station multimedia receive channel.
pub struct OpenMultimediaChannel {
    pub conference_id: ConferenceId,
    pub passthrough_party_id: crate::types::PassthroughPartyId,
    pub line_instance: u32,
    pub call_reference: CallReference,
    pub payload: MultimediaPayload,
    pub conference_creator: bool,
    /// Optional SRTP parameters carried by extended layouts.
    pub encryption: Option<MediaEncryption>,
    /// Identity for this media stream within the conference.
    pub stream_passthrough_id: u32,
    /// Related stream identity, or zero when the stream is independent.
    pub associated_stream_id: u32,
    pub source: MediaEndpointAddress,
    pub requested_address_type: IpAddressType,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Request to transmit a multimedia stream to a remote endpoint.
pub struct StartMultimediaTransmission {
    pub conference_id: ConferenceId,
    pub passthrough_party_id: crate::types::PassthroughPartyId,
    pub endpoint: MediaEndpointAddress,
    pub call_reference: CallReference,
    pub payload: MultimediaPayload,
    pub traffic_class: crate::types::MediaTrafficClass,
    /// Optional SRTP parameters carried by extended layouts.
    pub encryption: Option<MediaEncryption>,
    /// Identity for this media stream within the conference.
    pub stream_passthrough_id: u32,
    /// Related stream identity, or zero when the stream is independent.
    pub associated_stream_id: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Codec-specific multimedia command and its bounded parameter block.
pub struct MiscellaneousCommand {
    pub conference_id: ConferenceId,
    pub passthrough_party_id: crate::types::PassthroughPartyId,
    pub call_reference: CallReference,
    pub command: values::MiscCommandType,
    /// Command-specific bytes bounded by the fixed parameter area.
    pub data: BoundedBytes<36>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Maximum-bit-rate update for one video stream.
pub struct VideoFlowControl {
    pub conference_id: ConferenceId,
    pub passthrough_party_id: crate::types::PassthroughPartyId,
    pub call_reference: CallReference,
    pub maximum_bit_rate: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// One signaling DTMF tone associated with a conference media party.
pub struct DtmfToneControl {
    pub tone: Tone,
    pub conference_id: ConferenceId,
    pub passthrough_party_id: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Identity returned by a DTMF payload subscribe/unsubscribe operation.
pub struct DtmfPayloadIdentity {
    /// RTP payload-type word assigned to telephone-event packets.
    pub payload_type: u32,
    pub conference_id: u32,
    pub passthrough_party_id: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Request to subscribe or unsubscribe a DTMF RTP payload mapping.
pub struct DtmfPayloadRequest {
    /// Requested RTP payload-type word for telephone-event packets.
    pub payload_type: u32,
    pub conference_id: u32,
    pub passthrough_party_id: u32,
    /// Numeric DTMF transport selector retained from the wire.
    pub dtmf_type: u32,
}

/// Maximum inbound XML-alarm payload retained by the decoder.
pub const XML_ALARM_MAX_WIRE_BYTES: usize = 2_048;
/// Deterministic payload size emitted by [`XmlAlarmMessage::from_xml`].
pub const XML_ALARM_CANONICAL_WIRE_BYTES: usize = 2_004;
/// Maximum XML document size accepted by [`XmlAlarmMessage::from_xml`].
pub const XML_ALARM_CANONICAL_DOCUMENT_BYTES: usize = 2_000;

#[derive(Clone, Debug, Eq, PartialEq)]
/// Bounded XML alarm with exact inbound wire-payload preservation.
///
/// [`Self::from_xml`] constructs the canonical zero-padded outbound form;
/// [`Self::from_wire_payload`] retains any accepted framed form byte-for-byte.
pub struct XmlAlarmMessage {
    wire_payload: BoundedBytes<XML_ALARM_MAX_WIRE_BYTES>,
}

impl XmlAlarmMessage {
    /// Builds the canonical outbound alarm payload from a NUL-free XML document.
    pub fn from_xml(xml: impl AsRef<[u8]>) -> Result<Self, CodecError> {
        let xml = xml.as_ref();
        if xml.contains(&0) {
            return Err(CodecError::InvalidText);
        }
        if xml.len() > XML_ALARM_CANONICAL_DOCUMENT_BYTES {
            return Err(CodecError::TextTooLong {
                message_id: wire_id::XML_ALARM,
                field: "alarm XML",
                actual: xml.len(),
                maximum: XML_ALARM_CANONICAL_DOCUMENT_BYTES,
            });
        }
        let mut wire_payload = vec![0; XML_ALARM_CANONICAL_WIRE_BYTES];
        wire_payload[..xml.len()].copy_from_slice(xml);
        Self::from_wire_payload(wire_payload)
    }

    /// Retains an inbound alarm payload without requiring a canonical length.
    pub fn from_wire_payload(payload: impl Into<Box<[u8]>>) -> Result<Self, CodecError> {
        let payload = payload.into();
        let wire_payload =
            BoundedBytes::new(payload).map_err(|error| CodecError::CountTooLarge {
                message_id: wire_id::XML_ALARM,
                field: "alarm payload",
                count: error.actual,
                maximum: error.maximum,
            })?;
        Ok(Self { wire_payload })
    }

    /// Returns the XML bytes through the first NUL, or the full payload if none exists.
    pub fn xml_bytes(&self) -> &[u8] {
        let bytes = self.wire_payload.as_bytes();
        let end = bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(bytes.len());
        &bytes[..end]
    }

    /// Returns the complete retained payload, including terminator and padding bytes.
    pub fn wire_payload(&self) -> &[u8] {
        self.wire_payload.as_bytes()
    }
}

/// Audio media-failure detector configuration.
///
/// The final four qualifier bytes are either a G.723 rate word or four
/// codec-specific bytes, depending on protocol version and codec. Keeping
/// them raw makes that union lossless without inventing a universal meaning.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MediaFailureDetection {
    pub conference_id: ConferenceId,
    pub passthrough_party_id: u32,
    pub packet_millis: u32,
    pub codec: Codec,
    pub echo_cancellation: EchoCancellation,
    pub codec_qualifier: [u8; 4],
    pub call_reference: CallReference,
}

/// All three integers and the text buffer have unknown semantics, so the typed
/// model preserves each value without assigning invented meaning.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionDeviceCapabilities {
    pub unknown_1: u32,
    pub unknown_2: u32,
    pub unknown_3: u32,
    pub description: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Static station and user information returned by a configuration request.
pub struct ConfigurationStatus {
    pub device_name: String,
    pub station_user_id: u32,
    pub station_instance: u32,
    pub line_count: u32,
    pub speed_dial_count: u32,
    pub user_name: String,
    pub server_name: String,
}

/// Messages exchanged with conference/media-resource/call-control peers.
///
/// These IDs share the SCCP frame header with station traffic, but they are
/// not legal inputs to [`ClientMessage`] or outputs from [`ServerMessage`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlMessage {
    MediaResourceNotification(MediaResourceNotification),
    PortResponse(PortEndpoint),
    StartSessionTransmission(SessionTransmission),
    StopSessionTransmission(SessionTransmission),
    ClearConference {
        conference_id: ConferenceId,
        service_number: u32,
    },
    CreateConferenceRequest(CreateConferenceRequest),
    DeleteConferenceRequest {
        conference_id: ConferenceId,
    },
    ModifyConferenceRequest(ModifyConferenceRequest),
    AddParticipantRequest(AddParticipantRequest),
    DropParticipantRequest {
        conference_id: ConferenceId,
        call_reference: CallReference,
    },
    AuditConferenceRequest,
    AuditParticipantRequest {
        conference_id: ConferenceId,
    },
    ChangeParticipantRequest(ChangeParticipantRequest),
    CreateConferenceResponse(CreateConferenceResponse),
    DeleteConferenceResponse {
        conference_id: ConferenceId,
        result: DeleteConferenceResult,
    },
    ModifyConferenceResponse(ModifyConferenceResponse),
    AddParticipantResponse(AddParticipantResponse),
    AuditConferenceResponse(AuditConferenceResponse),
    AuditParticipantResponse(AuditParticipantResponse),
    /// Plays a bounded sequence of locale-aware tones for conference parties.
    StartAnnouncement {
        announcements: Vec<AnnouncementEntry>,
        /// Whether completion requires a protocol acknowledgement.
        end_of_ack: EndOfAnnouncementAck,
        conference_id: u32,
        /// Party identifiers participating in the announcement matrix.
        matrix_conference_party_ids: Vec<u32>,
        /// Bit mask selecting which matrix parties hear the announcement.
        hearing_conference_party_mask: u32,
        play_mode: AnnouncementPlayMode,
    },
    StopAnnouncement {
        conference_id: u32,
    },
    AnnouncementFinish {
        conference_id: u32,
        play_status: AnnouncementPlayStatus,
    },
    QosReservationNotify {
        flow: QosFlow,
        direction: QosDirection,
    },
    /// Reports admission or reservation failure details for a media flow.
    QosErrorNotify {
        flow: QosFlow,
        direction: QosDirection,
        error_code: QosErrorCode,
        /// Network node that originated the RSVP error.
        failure_node: Ipv4Addr,
        rsvp_error_code: RsvpErrorCode,
        rsvp_error_subcode: u32,
        rsvp_error_flags: u32,
    },
    /// Establishes an RSVP listener and its retry/admission policy.
    QosListen {
        flow: QosFlow,
        reservation_style: QosReservationStyle,
        maximum_retries: u32,
        retry_timer: u32,
        /// Whether the service node must confirm successful reservation.
        confirmation_required: bool,
        /// Priority used when competing reservations may be preempted.
        preemption_priority: u32,
        /// Priority used when defending this reservation from preemption.
        defending_priority: u32,
        traffic: QosTrafficSpecification,
        application: QosApplicationIdentifier,
    },
    /// Establishes the sending side of an RSVP path.
    QosPath {
        flow: QosFlow,
        reservation_style: QosReservationStyle,
        maximum_retries: u32,
        retry_timer: u32,
        preemption_priority: u32,
        defending_priority: u32,
        traffic: QosTrafficSpecification,
        application: QosApplicationIdentifier,
    },
    /// Tears down QoS state for one direction of a media flow.
    QosTeardown {
        flow: QosFlow,
        direction: QosDirection,
    },
    /// Updates the six-bit DSCP value for a media flow.
    UpdateDscp {
        flow: QosFlow,
        dscp: u8,
    },
    /// Changes traffic parameters on an existing QoS reservation.
    QosModify {
        flow: QosFlow,
        direction: QosDirection,
        traffic: QosTrafficSpecification,
        application: QosApplicationIdentifier,
    },
    MessageWaitingNotification(MessageWaitingNotification),
    MessageWaitingResponse {
        target_number: String,
        result: MessageWaitingResult,
    },
    /// A documented role whose payload layout is not independently stable.
    KnownOpaque(KnownOpaqueMessage),
}

/// Maximum retained station quality-statistics payload.
pub const CONNECTION_QUALITY_MAX_BYTES: usize = 600;

/// Bounded, owned station quality data retained for the typed MED-019 parser.
///
/// Firmware can place arbitrary text in this field, so diagnostics deliberately
/// expose only its length.
#[derive(Clone, Eq, PartialEq)]
pub struct ConnectionQualityStatistics(Vec<u8>);

impl ConnectionQualityStatistics {
    /// Retains quality bytes when they fit the protocol allocation bound.
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, CodecError> {
        let bytes = bytes.into();
        if bytes.len() > CONNECTION_QUALITY_MAX_BYTES {
            return Err(CodecError::CountTooLarge {
                message_id: wire_id::CONNECTION_STATISTICS_RES,
                field: "quality statistics",
                count: bytes.len(),
                maximum: CONNECTION_QUALITY_MAX_BYTES,
            });
        }
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for ConnectionQualityStatistics {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConnectionQualityStatistics")
            .field("byte_count", &self.0.len())
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
/// Packet, octet, timing, and station-provided quality statistics for a call.
///
/// Debug output redacts the directory number and the nested quality payload.
pub struct ConnectionStatistics {
    pub directory_number: String,
    pub call_reference: u32,
    pub processing: StatisticsProcessing,
    pub packets_sent: u32,
    pub octets_sent: u32,
    pub packets_received: u32,
    pub octets_received: u32,
    pub packets_lost: u32,
    /// Inter-arrival jitter in milliseconds.
    pub jitter_millis: u32,
    /// Reported media latency in milliseconds.
    pub latency_millis: u32,
    pub quality: ConnectionQualityStatistics,
}

impl fmt::Debug for ConnectionStatistics {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConnectionStatistics")
            .field("directory_number", &"<redacted>")
            .field("call_reference", &self.call_reference)
            .field("processing", &self.processing)
            .field("packets_sent", &self.packets_sent)
            .field("octets_sent", &self.octets_sent)
            .field("packets_received", &self.packets_received)
            .field("octets_received", &self.octets_received)
            .field("packets_lost", &self.packets_lost)
            .field("jitter_millis", &self.jitter_millis)
            .field("latency_millis", &self.latency_millis)
            .field("quality", &self.quality)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Optional eight-byte extension retained from a media-transmission ACK.
pub struct MediaTransmissionAckWire {
    /// Extension present only in the longer selected ACK layout.
    pub extension: Option<[u8; 8]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Station acknowledgement for an audio media-transmission request.
pub struct MediaTransmissionAck {
    pub conference_id: u32,
    pub passthrough_party_id: u32,
    pub call_reference: u32,
    pub status: MediaStatus,
    pub address: IpAddr,
    pub port: u16,
    /// Optional layout-specific bytes needed for lossless re-encoding.
    pub wire: Option<MediaTransmissionAckWire>,
}

/// Fields in OpenReceiveChannel which are not part of the runtime media
/// abstraction but are required for byte-exact capture round trips.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenReceiveChannelWire {
    pub conference_id: u32,
    /// Codec qualifier word used as the G.723 bit-rate selector when applicable.
    pub g723_bitrate: u32,
    /// Identity for this media stream within the conference.
    pub stream_passthrough_id: u32,
    /// Related stream identity, or zero when the stream is independent.
    pub associated_stream_id: u32,
    /// Numeric DTMF transport selector retained from the wire.
    pub dtmf_type: u32,
    /// Conference mixer mode retained from the selected layout.
    pub mixing_mode: u32,
    /// Media-direction word retained from the selected layout.
    pub direction: u32,
    /// Requested address-family word retained from the selected layout.
    pub requested_address_type: u32,
    /// Station audio-level adjustment retained from the selected layout.
    pub audio_level_adjustment: u32,
    /// Fixed latent-capability area retained byte-for-byte.
    pub latent_capabilities: [u8; 36],
}

/// Fields in StartMediaTransmission which are deliberately kept separate
/// from the runtime RTP endpoint but must not be discarded by the codec.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartMediaTransmissionWire {
    pub conference_id: u32,
    /// Codec qualifier word used as the G.723 bit-rate selector when applicable.
    pub g723_bitrate: u32,
    /// Identity for this media stream within the conference.
    pub stream_passthrough_id: u32,
    /// Related stream identity, or zero when the stream is independent.
    pub associated_stream_id: u32,
    /// Numeric DTMF transport selector retained from the wire.
    pub dtmf_type: u32,
    /// Conference mixer mode retained from the selected layout.
    pub mixing_mode: u32,
    /// Media-direction word retained from the selected layout.
    pub direction: u32,
    /// Fixed latent-capability area retained byte-for-byte.
    pub latent_capabilities: [u8; 36],
}

/// Non-canonical phone-originated keypad bodies selected by their exact body
/// length. `None` on `ClientMessage::KeypadButton` emits the current extended
/// layout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeypadButtonWireLayout {
    /// Four-byte body carrying only the keypad value.
    LegacyButtonOnly,
    /// Twelve-byte body carrying keypad value, line, and call identity.
    WithCallIdentity,
}

/// One physical position in a station button template.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ButtonTemplateEntry {
    pub instance: u32,
    pub button_type: ButtonType,
}

impl Default for ButtonTemplateEntry {
    fn default() -> Self {
        Self {
            instance: 0,
            button_type: ButtonType::Unused,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Typed messages accepted from a station connection.
///
/// Variants correspond to station-to-control identifiers in
/// [`catalog::MessageId`]. [`KnownOpaque`](Self::KnownOpaque) retains a known
/// catalog entry without a typed payload, while [`Unknown`](Self::Unknown)
/// retains an unrecognized identifier. Decode with [`Self::decode`] during
/// registration and [`Self::decode_with_version`] after version negotiation.
pub enum ClientMessage {
    /// Reports that the station connection remains active.
    /// Refreshes the server-side keepalive deadline.
    KeepAlive,
    /// Introduces a station and its requested SCCP protocol characteristics.
    /// Starts registration and device authentication on the server.
    Register(RegistrationMessage),
    /// Reports the UDP port on which the station expects media.
    /// Supplies the station RTP port to server-side media setup.
    IpPort { rtp_port: u16 },
    /// Reports a digit pressed on the station keypad.
    /// Associates the input with its line and call when the wire layout permits.
    KeypadButton {
        button: Digit,
        line_instance: u32,
        call_reference: u32,
        wire_layout: Option<KeypadButtonWireLayout>,
    },
    /// Submits a complete called-party number in one message.
    /// Requests call setup without sending digits individually.
    EnblocCall {
        called_party: String,
        line_instance: u32,
    },
    /// Reports a physical or logical station button stimulus.
    /// Identifies the affected instance, call, and stimulus status.
    Stimulus {
        stimulus: Stimulus,
        instance: u32,
        call_reference: u32,
        status: u32,
    },
    /// Reports that the station handset or audio path went off hook.
    /// Starts or resumes call handling for the identified line and call.
    OffHook {
        line_instance: u32,
        call_reference: u32,
    },
    /// Reports that the station handset or audio path went on hook.
    /// Ends or releases call handling for the identified line and call.
    OnHook {
        line_instance: u32,
        call_reference: u32,
    },
    /// Reports an off-hook transition with originating-party details.
    /// Carries the calling number, mailbox, and selected line instance.
    OffHookWithCallingParty {
        calling_party_number: String,
        voice_mailbox: String,
        line_instance: u32,
    },
    /// Requests the configured status of one station line.
    /// Uses the line instance to select the directory number response.
    LineStatRequest { line_instance: u32 },
    /// Requests the station's current device configuration.
    /// Prompts the server to return its configuration status.
    ConfigStatRequest,
    /// Requests the server's current date and time.
    /// Prompts synchronization of the station clock.
    TimeDateRequest,
    /// Requests the station's provisioned button layout.
    /// Prompts one or more button-template chunks from the server.
    ButtonTemplateRequest,
    /// Requests the firmware or load version assigned to the station.
    /// Prompts the server's version response during provisioning.
    VersionRequest,
    /// Reports the station's supported media capabilities.
    /// Answers a server capability request with codec and media details.
    CapabilitiesResponse(Vec<MediaCapability>),
    /// Reports the RTP ports the station has allocated for media streams.
    /// Carries up to sixteen port numbers for use by call control.
    MediaPortList(MediaPortList),
    /// Reports a changed set of station media capabilities.
    /// Lets the server refresh capabilities after initial negotiation.
    CapabilitiesUpdate(CapabilityUpdate),
    /// Acknowledges opening a multimedia receive channel.
    /// Reports the resulting endpoint and status for the requested stream.
    OpenMultimediaReceiveChannelAck(OpenMultimediaReceiveChannelAck),
    /// Requests the station's configured signaling-server list.
    /// Prompts the server response used for failover discovery.
    ServerRequest,
    /// Reports a station alarm or diagnostic condition.
    /// Carries severity, text, and optional vendor parameter words.
    Alarm {
        severity: AlarmSeverity,
        text: String,
        /// Optional alarm parameter words. `None` preserves the shorter wire
        /// layout exactly.
        parameters: Option<[u32; 2]>,
    },
    /// Acknowledges starting multicast media reception.
    /// Reports status for the passthrough party and call.
    MulticastMediaReceptionAck {
        status: MediaStatus,
        passthrough_party_id: crate::types::PassthroughPartyId,
        call_reference: CallReference,
    },
    /// Acknowledges opening an audio receive channel.
    /// Reports status and the station's selected media endpoint.
    OpenReceiveChannelAck {
        status: MediaStatus,
        address: IpAddr,
        port: u16,
        passthrough_party_id: u32,
        call_reference: u32,
    },
    /// Requests the soft-key sets available to the station.
    /// Prompts the server to send the active soft-key profile.
    SoftKeySetRequest,
    /// Requests the station's soft-key action template.
    /// Prompts the server to enumerate supported soft-key actions.
    SoftKeyTemplateRequest,
    /// Reports activation of a station soft key.
    /// Associates the action with its line and call context.
    SoftKeyEvent {
        event: u32,
        line_instance: u32,
        call_reference: u32,
    },
    /// Requests removal of the station registration.
    /// Carries the station-provided reason for ending the session.
    Unregister { reason: u32 },
    /// Requests a registration token before full station registration.
    /// Supplies the station identity used for token admission control.
    RegisterToken(RegisterTokenMessage),
    /// Requests an SPCP registration token before full station registration.
    /// Supplies the station identity, address, device type, and stream capacity.
    SpcpRegisterToken(SpcpRegisterTokenMessage),
    /// Reports a hook-flash action on an analog-style call.
    /// Associates the flash with its line and call context.
    HookFlash {
        line_instance: u32,
        call_reference: u32,
    },
    /// Requests call-forwarding state for one line.
    /// Prompts the server to return configured forwarding destinations.
    ForwardStatusRequest { line_instance: u32 },
    /// Requests the contents of one speed-dial entry.
    /// Uses the speed-dial instance to select the response.
    SpeedDialStatusRequest { speed_dial_instance: u32 },
    /// Returns media connection statistics collected by the station.
    /// Carries packet, jitter, latency, and quality data for a call.
    ConnectionStatisticsResponse(ConnectionStatistics),
    /// Reports whether the station headset is enabled.
    /// Updates the server's view of the station audio accessory state.
    HeadsetStatus { enabled: bool },
    /// Reports a station media-resource state change.
    /// Carries the resource type, direction, and availability details.
    MediaResourceNotification(MediaResourceNotification),
    /// Reports an event on a particular station media path.
    /// Identifies both the media path and the observed event.
    MediaPathEvent {
        path: MediaPathId,
        event: MediaPathEvent,
    },
    /// Reports a capability of a particular station media path.
    /// Identifies the path and its supported media behavior.
    MediaPathCapability {
        path: MediaPathId,
        capability: MediaPathCapability,
    },
    /// Reports failure of an active media transmission.
    /// Identifies the stream endpoint, call, and failure status.
    MediaTransmissionFailure {
        conference_id: u32,
        passthrough_party_id: u32,
        address: IpAddr,
        port: u16,
        call_reference: u32,
        status: MediaStatus,
    },
    /// Reports how many line appearances the station can register.
    /// Lets the server constrain provisioning to the station's capacity.
    RegisterAvailableLines { lines: u32 },
    /// Requests the configured service URL at an index.
    /// Prompts the server to return its URL, label, and extension text.
    ServiceUrlStatusRequest { index: u32 },
    /// Requests the state of a provisioned feature button.
    /// Carries the feature index and station capability bits.
    FeatureStatusRequest {
        index: u32,
        /// Station feature-capability bits included in the request layout.
        capabilities: u32,
    },
    /// Acknowledges a request to start audio transmission.
    /// Reports the station's result for the requested media stream.
    StartMediaTransmissionAck(MediaTransmissionAck),
    /// Acknowledges a request to start multimedia transmission.
    /// Reports the station's result for the requested video or data stream.
    StartMultimediaTransmissionAck(StartMultimediaTransmissionAck),
    /// Reports capabilities supplied by an attached extension device.
    /// Lets the server account for expansion-module and accessory features.
    ExtensionDeviceCapabilities(ExtensionDeviceCapabilities),
    /// Carries legacy application data from the station to the server.
    /// Uses the fixed-format device-to-user data layout.
    DeviceToUserData(UserDataMessage),
    /// Returns a legacy station response to server application data.
    /// Uses the fixed-format device-to-user response layout.
    DeviceToUserDataResponse(UserDataMessage),
    /// Carries version-one application data from the station.
    /// Supports the extended variable-length user-data layout.
    DeviceToUserDataV1(UserDataV1Message),
    /// Returns a version-one station response to application data.
    /// Supports the extended variable-length response layout.
    DeviceToUserDataResponseV1(UserDataV1Message),
    /// Returns endpoint information requested for a station port.
    /// Identifies the address and port selected by the station.
    PortResponse(PortEndpoint),
    /// Requests status for a station feature subscription.
    /// Carries the transaction and feature identifiers being queried.
    SubscriptionStatusRequest(SubscriptionRequest),
    /// Acknowledges subscription to an RTP DTMF payload.
    /// Identifies the negotiated payload associated with the subscription.
    SubscribeDtmfPayloadResponse(DtmfPayloadIdentity),
    /// Acknowledges removal of an RTP DTMF payload subscription.
    /// Identifies the payload whose subscription was removed.
    UnsubscribeDtmfPayloadResponse(DtmfPayloadIdentity),
    /// Reports the station's location information as XML.
    /// Provides location metadata for routing and emergency services.
    LocationInfo {
        /// Location XML limited to 2,400 bytes before its required terminator.
        xml: String,
    },
    /// Reports a structured station alarm encoded as XML.
    /// Carries the alarm payload and its associated station metadata.
    XmlAlarm(XmlAlarmMessage),
    /// Requests the server's current call-count information.
    CallCountRequest(CallCountRequestPayload),
    /// Returns the result of creating a conference.
    /// Correlates the station outcome with the requested conference.
    CreateConferenceResponse(CreateConferenceResponse),
    /// Returns the result of deleting a conference.
    /// Identifies the conference and its deletion result.
    DeleteConferenceResponse {
        conference_id: ConferenceId,
        result: DeleteConferenceResult,
    },
    /// Returns the result of modifying a conference.
    /// Carries the station outcome for the requested conference changes.
    ModifyConferenceResponse(ModifyConferenceResponse),
    /// Returns station state for a conference audit.
    /// Reports the conference details requested by the server.
    AuditConferenceResponse(AuditConferenceResponse),
    /// Returns the result of adding a conference participant.
    /// Identifies the conference participant and operation outcome.
    AddParticipantResponse(AddParticipantResponse),
    /// Returns station state for a participant audit.
    /// Reports the participant details requested by the server.
    AuditParticipantResponse(AuditParticipantResponse),
    /// Preserves a recognized station-to-server message without typed decoding.
    /// Retains its catalog identifier and payload bytes for lossless handling.
    KnownOpaque(KnownOpaqueMessage),
    /// Preserves an unrecognized station-to-server message.
    /// Retains the unknown identifier and raw payload for diagnostics or forwarding.
    Unknown(RawMessage),
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Typed messages emitted toward a station connection.
///
/// Use [`Self::encode_for_session`] after registration so both protocol version
/// and negotiated feature bits participate in layout selection. The simpler
/// [`Self::encode`] applies version-only selection. User-visible strings can be
/// encoded through the explicit legacy-code-page entry points when required.
pub enum ServerMessage {
    /// Accepts station registration and supplies session parameters.
    /// Carries keepalive intervals, protocol features, and the date template.
    RegisterAck {
        keepalive_seconds: u32,
        secondary_keepalive_seconds: u32,
        protocol: ProtocolVersion,
        features: PhoneFeatures,
        date_template: DateTemplate,
    },
    /// Rejects a station registration attempt.
    /// Returns a human-readable reason for refusing the registration.
    RegisterReject { reason: String },
    /// Acknowledges a station keepalive message.
    /// Confirms that the signaling session remains active.
    KeepAliveAck,
    /// Acknowledges a station unregister request.
    /// Confirms that the server has released the registration.
    UnregisterAck,
    /// Requests the station's supported media capabilities.
    /// Prompts a capability response used for media negotiation.
    CapabilitiesRequest,
    /// Invokes the station's legacy announcement enunciator.
    /// Applies to the station rather than a particular line or call.
    EnunciatorCommand,
    /// Supplies the station's provisioned device configuration.
    /// Carries user, service, and device settings needed after registration.
    ConfigStatus(ConfigurationStatus),
    /// Supplies the configured identity and button label of one station line.
    /// Keeps the directory number, header identity, and button text distinct.
    LineStatus {
        instance: u32,
        directory_number: String,
        fully_qualified_display_name: String,
        display_label: String,
    },
    /// Supplies one chunk of the station's logical button layout.
    /// Uses offset and total counts to span layouts across multiple frames.
    ButtonTemplate {
        offset: u32,
        total: u32,
        buttons: Vec<ButtonTemplateEntry>,
    },
    /// Supplies the firmware or load version assigned to the station.
    /// Answers the station's version request during provisioning.
    Version { firmware: String },
    /// Supplies the station's signaling-server list.
    /// Provides primary and failover endpoints for server discovery.
    ServerResponse {
        servers: Vec<SignalingServerEndpoint>,
    },
    /// Supplies the server's current date and time.
    /// Synchronizes the station clock with calendar and Unix time fields.
    TimeDate {
        year: u32,
        month: u32,
        weekday: u32,
        day: u32,
        hour: u32,
        minute: u32,
        second: u32,
        milliseconds: u32,
        unix_seconds: u32,
    },
    /// Supplies the soft-key actions supported by the server.
    /// Defines the action identifiers referenced by soft-key sets.
    SoftKeyTemplate { actions: Vec<values::SoftKey> },
    /// Supplies the station's soft-key set profile.
    /// Maps call modes to ordered soft-key action lists.
    SoftKeySet { profile: SoftKeyProfile },
    /// Selects the soft-key set shown for a call.
    /// Uses a validity mask to enable positions in the selected set.
    SelectSoftKeys {
        line_instance: u32,
        call_reference: u32,
        set: KeyMode,
        /// Bit mask over positions in the selected soft-key set.
        valid_mask: u32,
    },
    /// Updates the station's state for a call appearance.
    /// Associates the new call state with a line and call reference.
    CallState {
        state: CallState,
        line_instance: u32,
        call_reference: u32,
    },
    /// Supplies calling and called party information for a call.
    /// Updates the station's call-information display and metadata.
    CallInfo {
        info: CallInfo,
        line_instance: u32,
        call_reference: u32,
    },
    /// Displays a call-specific prompt on the station.
    /// Sets its text, timeout, line, and call context.
    DisplayPrompt {
        timeout_seconds: u32,
        text: String,
        line_instance: u32,
        call_reference: u32,
    },
    /// Clears a call-specific prompt from the station.
    /// Targets the prompt associated with a line and call reference.
    ClearPrompt {
        line_instance: u32,
        call_reference: u32,
    },
    /// Displays a transient notification on the station.
    /// Supplies notification text and its timeout.
    DisplayNotify { timeout_seconds: u32, text: String },
    /// Clears the station's transient notification.
    /// Removes the notification created by a display-notify message.
    ClearNotify,
    /// Displays a prioritized transient notification.
    /// Supplies its priority, text, and timeout.
    DisplayPriorityNotify {
        timeout_seconds: u32,
        priority: NotificationPriority,
        text: String,
    },
    /// Clears notifications at a specified priority.
    /// Leaves notifications at other priorities unaffected.
    ClearPriorityNotify { priority: NotificationPriority },
    /// Notifies the station of a DTMF tone state.
    /// Carries digit and call context without requesting local tone generation.
    NotifyDtmfTone(DtmfToneControl),
    /// Instructs the station to send a DTMF tone.
    /// Carries the digit and call context for media signaling.
    SendDtmfTone(DtmfToneControl),
    /// Starts an announcement for a conference.
    /// Carries the announcement sequence, participants, mask, and play mode.
    StartAnnouncement {
        announcements: Vec<AnnouncementEntry>,
        end_of_ack: u32,
        conference_id: u32,
        matrix_conference_party_ids: Vec<u32>,
        hearing_conference_party_mask: u32,
        play_mode: u32,
    },
    /// Stops the active announcement for a conference.
    /// Targets the announcement by conference identifier.
    StopAnnouncement { conference_id: u32 },
    /// Reports or confirms announcement completion to the station.
    /// Carries the conference identifier and final play status.
    AnnouncementFinish {
        conference_id: u32,
        play_status: u32,
    },
    /// Clears conference state maintained by the station.
    /// Identifies the conference and associated service number.
    ClearConference {
        conference_id: ConferenceId,
        service_number: u32,
    },
    /// Requests creation of a station-managed conference.
    /// Carries the conference attributes and correlation identifiers.
    CreateConferenceRequest(CreateConferenceRequest),
    /// Requests deletion of a station-managed conference.
    /// Targets the conference by identifier.
    DeleteConferenceRequest { conference_id: ConferenceId },
    /// Requests changes to a station-managed conference.
    /// Carries the updated conference attributes and identifiers.
    ModifyConferenceRequest(ModifyConferenceRequest),
    /// Requests the station's current conference state.
    /// Prompts an audit response for conference reconciliation.
    AuditConferenceRequest,
    /// Requests adding a participant to a conference.
    /// Carries the conference, call, and participant details.
    AddParticipantRequest(AddParticipantRequest),
    /// Requests removal of a participant from a conference.
    /// Targets the participant by conference and call reference.
    DropParticipantRequest {
        conference_id: ConferenceId,
        call_reference: CallReference,
    },
    /// Requests the station's state for conference participants.
    /// Targets the participant set associated with a conference.
    AuditParticipantRequest { conference_id: ConferenceId },
    /// Requests changes to a conference participant.
    /// Carries updated participant attributes and identifiers.
    ChangeParticipantRequest(ChangeParticipantRequest),
    /// Stops a station multimedia transmit stream.
    /// Identifies the conference, party, and call owning the stream.
    StopMultimediaTransmission(MultimediaStreamControl),
    /// Commands a station video stream to change its flow.
    /// Carries the requested bit rate and stream identifiers.
    FlowControlCommand(VideoFlowControl),
    /// Closes a station multimedia receive channel.
    /// Identifies the conference, party, and call owning the channel.
    CloseMultimediaReceiveChannel(MultimediaStreamControl),
    /// Selects the station's video display layout for a call.
    /// Carries conference, call, and layout identifiers.
    VideoDisplayCommand {
        conference_id: ConferenceId,
        call_reference: CallReference,
        layout_id: u32,
    },
    /// Notifies the station of video flow-control state.
    /// Reports stream identifiers and the applicable bit rate.
    FlowControlNotify(VideoFlowControl),
    /// Activates the station call-control plane for a line.
    /// Makes the selected line instance the active call plane.
    ActivateCallPlane { line_instance: u32 },
    /// Deactivates the station call-control plane.
    /// Removes the currently active call-plane selection.
    DeactivateCallPlane,
    /// Confirms processing of a dial-string backspace.
    /// Associates the response with its line and call context.
    BackspaceResponse {
        line_instance: u32,
        call_reference: u32,
    },
    /// Accepts a station registration-token request.
    /// Allows the station to proceed with full registration.
    RegisterTokenAck,
    /// Rejects a station registration-token request.
    /// Supplies the delay before the station should retry.
    RegisterTokenReject { backoff_seconds: u32 },
    /// Accepts an SPCP registration-token request with a feature word.
    /// Allows the station to continue its SPCP registration sequence.
    SpcpRegisterTokenAck { features: u32 },
    /// Rejects an SPCP registration-token request temporarily.
    /// Supplies the delay before the station should request another token.
    SpcpRegisterTokenReject { backoff_seconds: u32 },
    /// Sets the station ringer behavior for a call.
    /// Carries mode, duration, line, and call context.
    SetRinger {
        mode: RingerMode,
        duration: RingDuration,
        line_instance: u32,
        call_reference: u32,
    },
    /// Sets the lamp state for a station button.
    /// Targets a button type and instance with the requested lamp mode.
    SetLamp {
        stimulus: ButtonType,
        instance: u32,
        mode: LampMode,
    },
    /// Enables hook-flash detection on stations that expose that capability.
    /// Causes subsequent hook-flash actions to be reported to call control.
    SetHookFlashDetect,
    /// Starts local tone generation on the station.
    /// Selects the tone, direction, line, and call context.
    StartTone {
        tone: Tone,
        direction: ToneDirection,
        line_instance: u32,
        call_reference: u32,
    },
    /// Stops local tone generation for a call.
    /// Targets the tone associated with a line and call reference.
    StopTone {
        line_instance: u32,
        call_reference: u32,
    },
    /// Starts station reception of a multicast media stream.
    /// Supplies multicast endpoint, codec, and stream identifiers.
    StartMulticastMediaReception(MulticastMediaReception),
    /// Starts station transmission to a multicast media stream.
    /// Supplies multicast endpoint, codec, and stream identifiers.
    StartMulticastMediaTransmission(MulticastMediaTransmission),
    /// Stops station reception of a multicast media stream.
    /// Targets the stream by conference, party, and call identifiers.
    StopMulticastMediaReception {
        conference_id: ConferenceId,
        passthrough_party_id: crate::types::PassthroughPartyId,
        call_reference: CallReference,
    },
    /// Stops station transmission to a multicast media stream.
    /// Targets the stream by conference, party, and call identifiers.
    StopMulticastMediaTransmission {
        conference_id: ConferenceId,
        passthrough_party_id: crate::types::PassthroughPartyId,
        call_reference: CallReference,
    },
    /// Opens a station audio receive channel.
    /// Supplies codec, packetization, source, encryption, and stream details.
    OpenReceiveChannel {
        call_reference: u32,
        passthrough_party_id: u32,
        packet_ms: u32,
        codec: Codec,
        echo_cancellation: EchoCancellation,
        /// Dynamic RTP payload type used for telephone-event DTMF, or zero for signaling DTMF.
        telephone_event_payload: u8,
        source_address: IpAddr,
        source_port: u16,
        encryption: Option<MediaEncryption>,
        /// Exact auxiliary wire fields, or encoder defaults when absent on a
        /// runtime-created message.
        wire: Option<OpenReceiveChannelWire>,
    },
    /// Closes a station audio receive channel.
    /// Identifies the conference, party, and call owning the stream.
    CloseReceiveChannel(AudioStreamControl),
    /// Requests media connection statistics from the station.
    /// Selects the call, directory number, and statistics processing mode.
    ConnectionStatisticsRequest {
        directory_number: String,
        call_reference: u32,
        processing: StatisticsProcessing,
    },
    /// Starts station audio transmission to a media endpoint.
    /// Supplies endpoint, traffic class, encryption, and stream details.
    StartMediaTransmission {
        call_reference: u32,
        passthrough_party_id: u32,
        endpoint: MediaEndpoint,
        silence_suppression: SilenceSuppression,
        /// Full traffic-class octet; configuration DSCP is shifted left by two.
        traffic_class: crate::types::MediaTrafficClass,
        encryption: Option<MediaEncryption>,
        /// Exact auxiliary wire fields, or encoder defaults when absent on a
        /// runtime-created message.
        wire: Option<StartMediaTransmissionWire>,
    },
    /// Stops a station audio transmit stream.
    /// Identifies the conference, party, and call owning the stream.
    StopMediaTransmission(AudioStreamControl),
    /// Starts the station's legacy receive-side media function.
    /// Carries no stream identity, endpoint, or codec parameters.
    StartMediaReception,
    /// Stops a legacy media-reception path for one conference party.
    /// Identifies the active reception by conference and passthrough party.
    StopMediaReception {
        conference_id: ConferenceId,
        passthrough_party_id: crate::types::PassthroughPartyId,
    },
    /// Requests subscription to an RTP DTMF payload.
    /// Carries the payload and transaction identity to subscribe.
    SubscribeDtmfPayloadRequest(DtmfPayloadRequest),
    /// Reports failure to establish a DTMF payload subscription.
    /// Identifies the payload and transaction that failed.
    SubscribeDtmfPayloadError(DtmfPayloadIdentity),
    /// Requests removal of an RTP DTMF payload subscription.
    /// Carries the payload and transaction identity to remove.
    UnsubscribeDtmfPayloadRequest(DtmfPayloadRequest),
    /// Reports failure to remove a DTMF payload subscription.
    /// Identifies the payload and transaction that failed.
    UnsubscribeDtmfPayloadError(DtmfPayloadIdentity),
    /// Sets the station speakerphone mode.
    /// Controls whether the station speaker audio path is active.
    SetSpeakerMode(SpeakerMode),
    /// Sets the station microphone mode.
    /// Controls whether the station microphone audio path is active.
    SetMicrophoneMode(MicrophoneMode),
    /// Requests a station reset or restart.
    /// Selects the reset behavior defined by the reset type.
    Reset(ResetType),
    /// Displays text in the station's general display area.
    /// Replaces the current non-call-specific display text.
    DisplayText { text: String },
    /// Clears the station's general display area.
    /// Removes text previously sent with a display-text message.
    ClearDisplay,
    /// Supplies call-forwarding state for one line.
    /// Carries destinations for all, busy, and no-answer forwarding.
    ForwardStatus {
        line_instance: u32,
        forward_all: Option<String>,
        forward_busy: Option<String>,
        forward_no_answer: Option<String>,
    },
    /// Supplies the contents of one station speed-dial entry.
    /// Maps an entry instance to its number and display name.
    SpeedDialStatus {
        instance: u32,
        number: String,
        display_name: String,
    },
    /// Displays or records the dialed number for a call.
    /// Associates the number with its line and call reference.
    DialedNumber {
        number: String,
        line_instance: u32,
        call_reference: u32,
    },
    /// Starts station monitoring for media-path failure.
    /// Supplies thresholds and stream identifiers used for detection.
    StartMediaFailureDetection(MediaFailureDetection),
    /// Carries legacy application data from the server to the station.
    /// Uses the fixed-format user-to-device data layout.
    UserToDeviceData(UserDataMessage),
    /// Carries version-one application data from the server.
    /// Supports the extended variable-length user-data layout.
    UserToDeviceDataV1(UserDataV1Message),
    /// Supplies the state of a provisioned feature button.
    /// Carries its type, label, instance, and feature-specific state.
    FeatureStatus {
        instance: u32,
        button_type: ButtonType,
        label: String,
        /// Feature-specific state word interpreted according to `button_type`.
        state: u32,
    },
    /// Supplies the configured service URL at an index.
    /// Carries its URL, label, and optional extension text.
    ServiceUrlStatus {
        index: u32,
        url: String,
        label: String,
        /// Additional dynamic-layout text; empty in layouts that do not carry it.
        extension_text: String,
    },
    /// Updates whether a call is selected on the station.
    /// Associates the selection state with its line and call.
    CallSelectStatus {
        /// Selection-state word retained as an extensible numeric value.
        status: u32,
        call_reference: u32,
        line_instance: u32,
    },
    /// Requests endpoint information for a station port.
    /// Carries the port identity and addressing parameters to resolve.
    PortRequest(PortRequest),
    /// Requests closure of a station port endpoint.
    /// Identifies the endpoint and port resources to release.
    PortClose(PortClose),
    /// Opens a station multimedia receive channel.
    /// Supplies codec, endpoint, and stream negotiation details.
    OpenMultimediaChannel(OpenMultimediaChannel),
    /// Starts station transmission of multimedia.
    /// Supplies destination, codec, bandwidth, and stream identifiers.
    StartMultimediaTransmission(StartMultimediaTransmission),
    /// Sends a stream-specific multimedia control command.
    /// Carries command data for video, picture, or recovery behavior.
    MiscellaneousCommand(MiscellaneousCommand),
    /// Supplies the result or state of a feature subscription.
    /// Carries transaction, feature, timer, and cause values.
    SubscriptionStatus {
        transaction_id: u32,
        feature_id: u32,
        timer_seconds: u32,
        cause: SubscriptionCause,
    },
    /// Sends a feature-subscription notification to the station.
    /// Carries transaction state, feature state, and display text.
    Notification {
        transaction_id: u32,
        feature_id: u32,
        status: BusyLampFieldState,
        text: String,
    },
    /// Sets how a call is represented in station call history.
    /// Associates the disposition with its line and call reference.
    CallHistoryDisposition {
        disposition: CallHistoryDisposition,
        line_instance: u32,
        call_reference: u32,
    },
    /// Returns the server's current per-line call capacity.
    CallCountResponse(CallCountResponse),
    /// Updates the station's recording indicator for a call.
    /// Carries the call reference and whether recording is active.
    RecordingStatus { call_reference: u32, active: bool },
    /// Preserves a recognized server-to-station message without typed decoding.
    /// Retains its catalog identifier and payload bytes for lossless handling.
    KnownOpaque(KnownOpaqueMessage),
    /// Preserves an unrecognized server-to-station message.
    /// Retains the unknown identifier and raw payload for diagnostics or forwarding.
    Unknown(RawMessage),
}

#[cfg(test)]
mod tests {
    use super::wire::{CodecError, Frame, FrameDecoder};
    use super::*;

    #[test]
    fn protocol_fillers_have_semantic_defaults() {
        assert_eq!(
            ButtonTemplateEntry::default(),
            ButtonTemplateEntry {
                instance: 0,
                button_type: ButtonType::Unused,
            }
        );
        assert_eq!(
            MessageWaitingCounts::default(),
            MessageWaitingCounts { new: 0, old: 0 }
        );
    }

    const fn test_rtp_payload_number(value: u32) -> RtpPayloadNumber {
        match RtpPayloadNumber::new(value) {
            Ok(value) => value,
            Err(_) => panic!("test RTP payload number is out of range"),
        }
    }

    fn decode_frame(bytes: &[u8]) -> Frame {
        FrameDecoder::new().push(bytes).unwrap().remove(0)
    }

    fn assert_contract_alignment(frame: &Frame) {
        use super::catalog::PayloadLayout;

        let contract = frame.message_type().contract().unwrap();
        if !matches!(
            contract.payload_layout,
            PayloadLayout::Opaque
                | PayloadLayout::BoundedOpaque
                | PayloadLayout::BoundedPreserved
                | PayloadLayout::VersionAndLengthSelected
                | PayloadLayout::MinimumLengthPreserved
        ) {
            assert_eq!(frame.payload.len() % 4, 0, "{}", contract.id);
        }
    }

    fn assert_client_round_trip(message: ClientMessage, protocol: ProtocolVersion) {
        let frame = decode_frame(&message.encode(protocol).unwrap());
        assert_contract_alignment(&frame);
        assert_eq!(
            ClientMessage::decode_with_version(frame, protocol).unwrap(),
            message
        );
    }

    fn assert_server_round_trip(message: ServerMessage, protocol: ProtocolVersion) {
        let frame = decode_frame(&message.encode(protocol).unwrap());
        assert_contract_alignment(&frame);
        assert_eq!(ServerMessage::decode(frame, protocol).unwrap(), message);
    }

    fn assert_control_round_trip(message: ControlMessage, protocol: ProtocolVersion) {
        let frame = decode_frame(&message.encode(protocol).unwrap());
        assert_contract_alignment(&frame);
        assert_eq!(ControlMessage::decode(frame, protocol).unwrap(), message);
    }

    #[test]
    fn multimedia_payload_exposes_only_typed_construction() {
        let capability = MultimediaVideoCapability::new(
            1_024,
            [MultimediaPictureFormat {
                format: VideoFormat::Cif4,
                minimum_picture_interval: 2,
            }],
            7,
            MultimediaVideoCapabilityArm::H264 {
                profile: 100,
                level: 42,
                custom_max_mbps: 40_500,
                custom_max_fs: 1_620,
                custom_max_dpb: 8_100,
                custom_max_br_and_cpb: 10_000,
            },
        )
        .unwrap();
        let payload = MultimediaPayload::new(test_rtp_payload_number(97), capability.clone());
        assert_eq!(payload.payload_number().get(), 97);
        assert_eq!(payload.descriptor().rfc_number(), 0);
        assert_eq!(payload.codec(), Codec::H264);
        assert_eq!(payload.video_capability(), Some(&capability));

        let packetized = MultimediaPayload::with_descriptor(
            MultimediaPayloadDescriptor::new(4, payload.payload_number()),
            capability.clone(),
        );
        assert_eq!(packetized.descriptor().rfc_number(), 4);
        assert_eq!(packetized.payload_number(), payload.payload_number());

        let debug = format!("{capability:?}");
        assert!(debug.contains("bit_rate: 1024"));
        assert!(!debug.contains("preserved_wire"));
        assert_eq!(
            RtpPayloadNumber::new(128),
            Err(RtpPayloadNumberError { actual: 128 })
        );
    }

    #[test]
    fn multimedia_picture_formats_are_bounded_before_payload_construction() {
        let formats = [MultimediaPictureFormat {
            format: VideoFormat::Cif,
            minimum_picture_interval: 1,
        }; MAX_MULTIMEDIA_PICTURE_FORMATS + 1];
        assert_eq!(
            MultimediaVideoCapability::new(
                1_024,
                formats,
                0,
                MultimediaVideoCapabilityArm::H261 {
                    temporal_spatial_trade_off_capability: 0,
                    still_image_transmission: 0,
                },
            )
            .unwrap_err(),
            MultimediaCapabilityError {
                maximum: MAX_MULTIMEDIA_PICTURE_FORMATS,
                actual: MAX_MULTIMEDIA_PICTURE_FORMATS + 1,
            }
        );
    }

    #[test]
    fn media_request_identity_is_nonzero_and_exhaustion_never_wraps() {
        assert_eq!(MediaRequestToken::new(0), None);
        let token = MediaRequestToken::new(7).unwrap();
        assert_eq!(MediaRequestIdentity::new(0, token), None);

        let first = MediaRequestIdentity::new(1, token).unwrap();
        let second = first.checked_next().unwrap();
        assert_eq!(second.generation(), 2);
        assert_eq!(second.token().get(), 8);

        assert_eq!(
            MediaRequestToken::new(u32::MAX).unwrap().checked_next(),
            None
        );
        let exhausted_generation =
            MediaRequestIdentity::new(u64::MAX, MediaRequestToken::new(1).unwrap()).unwrap();
        assert_eq!(exhausted_generation.checked_next(), None);
    }

    #[test]
    fn media_request_identity_matches_only_the_current_wire_token() {
        let identity =
            MediaRequestIdentity::new(2, MediaRequestToken::new(0x1020_3040).unwrap()).unwrap();

        assert!(identity.accepts_ack(0x1020_3040, 0, 77));
        assert!(identity.accepts_ack(0x1020_3040, 77, 77));
        assert!(!identity.accepts_ack(0x1020_3040, 78, 77));
        assert!(!identity.accepts_ack(0x1020_303f, 77, 77));
    }

    #[test]
    fn zero_party_fallback_cannot_settle_a_reopened_media_generation() {
        let first = MediaRequestIdentity::new(1, MediaRequestToken::new(700).unwrap()).unwrap();
        let reopened = first.checked_next().unwrap();

        // A zero-party ACK must carry the stable call reference.
        assert!(first.accepts_ack(0, 42, 42));
        assert!(!first.accepts_ack(0, 0, 42));

        // The same delayed ACK is ambiguous after a reopen and fails closed.
        assert!(!reopened.accepts_ack(0, 42, 42));
        assert!(!reopened.accepts_ack(first.token().get(), 42, 42));
        assert!(reopened.accepts_ack(reopened.token().get(), 42, 42));
    }

    #[test]
    fn decodes_7962_off_hook_capture_shape() {
        let frame = Frame::new(22, wire_id::OFF_HOOK, vec![1, 0, 0, 0, 42, 0, 0, 0]);
        assert_eq!(
            ClientMessage::decode(frame).unwrap(),
            ClientMessage::OffHook {
                line_instance: 1,
                call_reference: 42
            }
        );
    }

    #[test]
    fn decodes_7961_v22_three_word_keypad_capture_shape() {
        let payload: Vec<_> = [8_u32, 1, 1]
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect();
        let frame = Frame::new(22, wire_id::KEYPAD_BUTTON, payload.clone());
        let decoded = ClientMessage::decode(frame).unwrap();
        assert_eq!(
            decoded,
            ClientMessage::KeypadButton {
                button: Digit::Number(8),
                line_instance: 1,
                call_reference: 1,
                wire_layout: Some(KeypadButtonWireLayout::WithCallIdentity),
            }
        );
        let encoded = FrameDecoder::new()
            .push(&decoded.encode(ProtocolVersion::V22).unwrap())
            .unwrap()
            .remove(0);
        assert_eq!(encoded.payload, payload);
    }

    #[test]
    fn register_ack_is_protocol_zero_and_has_expected_fields() {
        let bytes = ServerMessage::RegisterAck {
            keepalive_seconds: 30,
            secondary_keepalive_seconds: 45,
            protocol: ProtocolVersion::V22,
            features: PhoneFeatures::UTF8 | PhoneFeatures::DYNAMIC_MESSAGES,
            date_template: DateTemplate::default(),
        }
        .encode(ProtocolVersion::V22)
        .unwrap();
        let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
        assert_eq!(frame.protocol_version, 0);
        assert_eq!(frame.message_id, wire_id::REGISTER_ACK);
        assert_eq!(
            ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
            ServerMessage::RegisterAck {
                keepalive_seconds: 30,
                secondary_keepalive_seconds: 45,
                protocol: ProtocolVersion::V22,
                features: PhoneFeatures::UTF8 | PhoneFeatures::DYNAMIC_MESSAGES,
                date_template: DateTemplate::default(),
            }
        );
    }

    #[test]
    fn media_layout_sizes_match_supported_wire_specs() {
        let endpoint = MediaEndpoint {
            address: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            rtp_port: 4000,
            rtcp_port: 4001,
            codec: Codec::Pcmu,
            packet_ms: 20,
            max_frames_per_packet: 1,
            telephone_event_payload: 101,
        };
        let start = ServerMessage::StartMediaTransmission {
            call_reference: 7,
            passthrough_party_id: 9,
            endpoint,
            silence_suppression: SilenceSuppression::Off,
            traffic_class: crate::types::MediaTrafficClass::from_wire(184),
            encryption: None,
            wire: None,
        }
        .encode(ProtocolVersion::V17)
        .unwrap();
        assert_eq!(start.len(), 144); // 12-byte header + 132-byte payload
        assert_eq!(&start[52..56], &184_u32.to_le_bytes());
        assert_eq!(&start[140..144], &1_u32.to_le_bytes());
        let open = ServerMessage::OpenReceiveChannel {
            call_reference: 7,
            passthrough_party_id: 9,
            packet_ms: 20,
            codec: Codec::Pcmu,
            echo_cancellation: EchoCancellation::On,
            telephone_event_payload: 101,
            source_address: endpoint.address,
            source_port: endpoint.rtp_port,
            encryption: None,
            wire: None,
        }
        .encode(ProtocolVersion::V17)
        .unwrap();
        assert_eq!(open.len(), 140); // 12-byte header + 128-byte payload
        assert_eq!(&open[108..112], &1_u32.to_le_bytes());

        let start_v3 = ServerMessage::StartMediaTransmission {
            call_reference: 7,
            passthrough_party_id: 9,
            endpoint,
            silence_suppression: SilenceSuppression::Off,
            traffic_class: crate::types::MediaTrafficClass::default(),
            encryption: None,
            wire: None,
        }
        .encode(ProtocolVersion::V3)
        .unwrap();
        assert_eq!(start_v3.len(), 120); // 12-byte header + 108-byte payload
        let open_v3 = ServerMessage::OpenReceiveChannel {
            call_reference: 7,
            passthrough_party_id: 9,
            packet_ms: 20,
            codec: Codec::Pcmu,
            echo_cancellation: EchoCancellation::On,
            telephone_event_payload: 101,
            source_address: endpoint.address,
            source_port: endpoint.rtp_port,
            encryption: None,
            wire: None,
        }
        .encode(ProtocolVersion::V3)
        .unwrap();
        assert_eq!(open_v3.len(), 104); // 12-byte header + 92-byte payload

        let start_v22 = ServerMessage::StartMediaTransmission {
            call_reference: 7,
            passthrough_party_id: 9,
            endpoint,
            silence_suppression: SilenceSuppression::Off,
            traffic_class: crate::types::MediaTrafficClass::default(),
            encryption: None,
            wire: None,
        }
        .encode(ProtocolVersion::V22)
        .unwrap();
        assert_eq!(start_v22.len(), 180); // 12-byte header + 168-byte payload
        let open_v22 = ServerMessage::OpenReceiveChannel {
            call_reference: 7,
            passthrough_party_id: 9,
            packet_ms: 20,
            codec: Codec::Pcmu,
            echo_cancellation: EchoCancellation::On,
            telephone_event_payload: 101,
            source_address: endpoint.address,
            source_port: endpoint.rtp_port,
            encryption: None,
            wire: None,
        }
        .encode(ProtocolVersion::V22)
        .unwrap();
        assert_eq!(open_v22.len(), 180); // 12-byte header + 168-byte payload
    }

    #[test]
    fn media_close_layouts_consume_the_reference_fields_exactly() {
        let close = ServerMessage::CloseReceiveChannel(AudioStreamControl {
            conference_id: 6.into(),
            passthrough_party_id: 9.into(),
            call_reference: 7.into(),
            port_handling_flag: 11,
        });
        let close_v3 = close.encode(ProtocolVersion::V3).unwrap();
        assert_eq!(close_v3.len(), 28);
        assert_eq!(
            ServerMessage::decode(decode_frame(&close_v3), ProtocolVersion::V3).unwrap(),
            close
        );
        let close_v5 = close.encode(ProtocolVersion::V5).unwrap();
        assert_eq!(close_v5.len(), 28);
        assert_eq!(
            ServerMessage::decode(decode_frame(&close_v5), ProtocolVersion::V5).unwrap(),
            close
        );

        let stop = ServerMessage::StopMediaTransmission(AudioStreamControl {
            conference_id: 6.into(),
            passthrough_party_id: 9.into(),
            call_reference: 7.into(),
            port_handling_flag: 11,
        });
        let bytes = stop.encode(ProtocolVersion::V22).unwrap();
        assert_eq!(bytes.len(), 28);
        assert_eq!(
            ServerMessage::decode(decode_frame(&bytes), ProtocolVersion::V22).unwrap(),
            stop
        );

        let mut trailing = decode_frame(&bytes);
        trailing.payload.extend_from_slice(&[0; 4]);
        assert!(matches!(
            ServerMessage::decode(trailing, ProtocolVersion::V22),
            Err(CodecError::TrailingBytes { count: 4, .. })
        ));
    }

    #[test]
    fn audio_packetization_round_trips_without_default_substitution() {
        let endpoint = MediaEndpoint {
            address: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)),
            rtp_port: 4000,
            rtcp_port: 4001,
            codec: Codec::G72264k,
            packet_ms: 30,
            max_frames_per_packet: 2,
            telephone_event_payload: 101,
        };
        for protocol in [
            ProtocolVersion::V3,
            ProtocolVersion::V17,
            ProtocolVersion::V22,
        ] {
            let (source_address, source_port) = if protocol.wire() < 12 {
                (IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
            } else {
                (endpoint.address, endpoint.rtp_port)
            };
            assert_server_round_trip(
                ServerMessage::OpenReceiveChannel {
                    call_reference: 7,
                    passthrough_party_id: 9,
                    packet_ms: 30,
                    codec: Codec::G72264k,
                    echo_cancellation: EchoCancellation::On,
                    telephone_event_payload: 101,
                    source_address,
                    source_port,
                    encryption: None,
                    wire: None,
                },
                protocol,
            );
            assert_server_round_trip(
                ServerMessage::StartMediaTransmission {
                    call_reference: 7,
                    passthrough_party_id: 9,
                    endpoint,
                    silence_suppression: SilenceSuppression::On,
                    traffic_class: crate::types::MediaTrafficClass::default(),
                    encryption: None,
                    wire: None,
                },
                protocol,
            );
            assert_client_round_trip(
                ClientMessage::MediaTransmissionFailure {
                    conference_id: 7,
                    passthrough_party_id: 9,
                    address: endpoint.address,
                    port: endpoint.rtp_port,
                    call_reference: 7,
                    status: MediaStatus::UnspecifiedError,
                },
                protocol,
            );
        }
    }

    #[test]
    fn ipv6_audio_endpoints_require_and_round_trip_extended_layouts() {
        let address: IpAddr = "2001:db8::42".parse().unwrap();
        let endpoint = MediaEndpoint {
            address,
            rtp_port: 40_000,
            rtcp_port: 40_001,
            codec: Codec::G72264k,
            packet_ms: 20,
            max_frames_per_packet: 1,
            telephone_event_payload: 101,
        };
        let start = ServerMessage::StartMediaTransmission {
            call_reference: 7,
            passthrough_party_id: 9,
            endpoint,
            silence_suppression: SilenceSuppression::Off,
            traffic_class: crate::types::MediaTrafficClass::default(),
            encryption: None,
            wire: None,
        };
        let receive_ack = ClientMessage::OpenReceiveChannelAck {
            status: MediaStatus::Ok,
            address,
            port: endpoint.rtp_port,
            passthrough_party_id: 9,
            call_reference: 7,
        };
        let transmit_ack = ClientMessage::StartMediaTransmissionAck(MediaTransmissionAck {
            conference_id: 6,
            passthrough_party_id: 9,
            call_reference: 7,
            status: MediaStatus::Ok,
            address,
            port: endpoint.rtp_port,
            wire: None,
        });
        let failure = ClientMessage::MediaTransmissionFailure {
            conference_id: 7,
            passthrough_party_id: 9,
            address,
            port: endpoint.rtp_port,
            call_reference: 7,
            status: MediaStatus::UnspecifiedError,
        };

        for protocol in [ProtocolVersion::V17, ProtocolVersion::V22] {
            assert_server_round_trip(start.clone(), protocol);
            assert_client_round_trip(receive_ack.clone(), protocol);
            assert_client_round_trip(transmit_ack.clone(), protocol);
            assert_client_round_trip(failure.clone(), protocol);
        }
        for result in [
            start.encode(ProtocolVersion::V16),
            receive_ack.encode(ProtocolVersion::V16),
            transmit_ack.encode(ProtocolVersion::V16),
            failure.encode(ProtocolVersion::V16),
            failure.encode(ProtocolVersion::V3),
        ] {
            assert!(matches!(
                result,
                Err(CodecError::InvalidValue {
                    field: "IP address family for pre-v17 protocol"
                        | "IP address family for this protocol version",
                    ..
                })
            ));
        }
    }

    #[test]
    fn skinny_dtmf_disables_the_telephone_event_payload_in_both_directions() {
        let endpoint = MediaEndpoint {
            address: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)),
            rtp_port: 4000,
            rtcp_port: 4001,
            codec: Codec::Pcmu,
            packet_ms: 20,
            max_frames_per_packet: 1,
            telephone_event_payload: 0,
        };
        for protocol in [
            ProtocolVersion::V3,
            ProtocolVersion::V17,
            ProtocolVersion::V22,
        ] {
            let (source_address, source_port) = if protocol.wire() < 12 {
                (IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
            } else {
                (endpoint.address, endpoint.rtp_port)
            };
            assert_server_round_trip(
                ServerMessage::OpenReceiveChannel {
                    call_reference: 7,
                    passthrough_party_id: 9,
                    packet_ms: 20,
                    codec: Codec::Pcmu,
                    echo_cancellation: EchoCancellation::On,
                    telephone_event_payload: 0,
                    source_address,
                    source_port,
                    encryption: None,
                    wire: None,
                },
                protocol,
            );
            assert_server_round_trip(
                ServerMessage::StartMediaTransmission {
                    call_reference: 7,
                    passthrough_party_id: 9,
                    endpoint,
                    silence_suppression: SilenceSuppression::Off,
                    traffic_class: crate::types::MediaTrafficClass::default(),
                    encryption: None,
                    wire: None,
                },
                protocol,
            );
        }
    }

    #[test]
    fn open_receive_wildcard_source_round_trips_for_all_supported_layouts() {
        for protocol in [
            ProtocolVersion::V3,
            ProtocolVersion::V17,
            ProtocolVersion::V22,
        ] {
            assert_server_round_trip(
                ServerMessage::OpenReceiveChannel {
                    call_reference: 1,
                    passthrough_party_id: 1,
                    packet_ms: 20,
                    codec: Codec::Pcma,
                    echo_cancellation: EchoCancellation::Off,
                    telephone_event_payload: 101,
                    source_address: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    source_port: 0,
                    encryption: None,
                    wire: None,
                },
                protocol,
            );
        }
    }

    #[test]
    fn media_encryption_round_trips_without_exposing_key_material() {
        let key = b"private-key-1234";
        let salt = b"private-salt-123";
        let encryption =
            MediaEncryption::new(EncryptionMethod::Aes128HmacSha1_80, key, salt, 1, 64).unwrap();
        assert_eq!(encryption.key(), key);
        assert_eq!(encryption.salt(), salt);

        let debug = format!("{encryption:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("112, 114, 105, 118, 97, 116, 101"));
        assert!(!debug.contains("private-key"));
        let endpoint = MediaEndpoint {
            address: IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)),
            rtp_port: 40_000,
            rtcp_port: 40_001,
            codec: Codec::Pcmu,
            packet_ms: 20,
            max_frames_per_packet: 1,
            telephone_event_payload: 101,
        };

        for protocol in [
            ProtocolVersion::new(12).unwrap(),
            ProtocolVersion::V17,
            ProtocolVersion::V22,
        ] {
            let open = ServerMessage::OpenReceiveChannel {
                call_reference: 7,
                passthrough_party_id: 9,
                packet_ms: 20,
                codec: Codec::Pcmu,
                echo_cancellation: EchoCancellation::On,
                telephone_event_payload: 101,
                source_address: endpoint.address,
                source_port: endpoint.rtp_port,
                encryption: Some(encryption.clone()),
                wire: None,
            };
            let open_debug = format!("{open:?}");
            assert!(open_debug.contains("<redacted>"));
            assert!(!open_debug.contains("112, 114, 105, 118, 97, 116, 101"));
            assert_server_round_trip(open, protocol);
            assert_server_round_trip(
                ServerMessage::StartMediaTransmission {
                    call_reference: 7,
                    passthrough_party_id: 9,
                    endpoint,
                    silence_suppression: SilenceSuppression::Off,
                    traffic_class: crate::types::MediaTrafficClass::default(),
                    encryption: Some(encryption.clone()),
                    wire: None,
                },
                protocol,
            );
        }
    }

    #[test]
    fn media_encryption_rejects_oversized_secrets_with_metadata_only_errors() {
        let oversized_key = [0xa5; 17];
        let error = MediaEncryption::new(
            EncryptionMethod::Aes128HmacSha1_32,
            &oversized_key,
            &[],
            0,
            0,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            CodecError::SecretTooLong {
                field: "media encryption key",
                actual: 17,
                maximum: 16,
            }
        ));
        assert!(!error.to_string().contains("165"));

        let oversized_salt = [0x5a; 17];
        let error = MediaEncryption::new(
            EncryptionMethod::Aes128HmacSha1_32,
            &[],
            &oversized_salt,
            0,
            0,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            CodecError::SecretTooLong {
                field: "media encryption salt",
                actual: 17,
                maximum: 16,
            }
        ));
        assert!(!error.to_string().contains("90"));
    }

    #[test]
    fn common_client_messages_round_trip_semantically() {
        assert_client_round_trip(
            ClientMessage::FeatureStatusRequest {
                index: 7,
                capabilities: 1,
            },
            ProtocolVersion::V22,
        );
        assert_client_round_trip(
            ClientMessage::OffHookWithCallingParty {
                calling_party_number: "1001".into(),
                voice_mailbox: "5001".into(),
                line_instance: 1,
            },
            ProtocolVersion::V3,
        );
        assert_client_round_trip(
            ClientMessage::RegisterToken(RegisterTokenMessage {
                device_id: DeviceId::new("SEP001122334455").unwrap(),
                device_instance: 2,
                address: "2001:db8::42".parse().unwrap(),
                device_type: DeviceType::Cisco7962,
                flags: 6,
            }),
            ProtocolVersion::V22,
        );
        assert_control_round_trip(
            ControlMessage::MediaResourceNotification(MediaResourceNotification {
                device_type: DeviceType::Unknown(0xfeed),
                in_service_streams: 2,
                max_streams_per_conference: 4,
                out_of_service_streams: 1,
            }),
            ProtocolVersion::V17,
        );
        assert_client_round_trip(
            ClientMessage::SubscriptionStatusRequest(SubscriptionRequest {
                transaction_id: 0x4b,
                feature_id: 1,
                timer_seconds: 30,
                subscription_id: "4000".into(),
            }),
            ProtocolVersion::V22,
        );
        for message in [
            ClientMessage::SubscribeDtmfPayloadResponse(DtmfPayloadIdentity {
                payload_type: 101,
                conference_id: 42,
                passthrough_party_id: 7,
            }),
            ClientMessage::UnsubscribeDtmfPayloadResponse(DtmfPayloadIdentity {
                payload_type: 102,
                conference_id: 43,
                passthrough_party_id: 8,
            }),
        ] {
            let encoded = message.encode(ProtocolVersion::V22).unwrap();
            let frame = decode_frame(&encoded);
            assert_eq!(frame.payload.len(), 12);
            assert_eq!(
                ClientMessage::decode_with_version(frame, ProtocolVersion::V22).unwrap(),
                message
            );
        }
        assert_client_round_trip(
            ClientMessage::DeviceToUserDataV1(UserDataV1Message {
                application_id: 7,
                line_instance: 1,
                call_reference: 42,
                transaction_id: 9,
                sequence_flag: 1,
                display_priority: 2,
                conference_id: 42,
                application_instance_id: 3,
                routing: 4,
                data: b"<CiscoIPPhoneText/>".to_vec(),
            }),
            ProtocolVersion::V17,
        );
        assert_client_round_trip(
            ClientMessage::DeviceToUserDataResponse(UserDataMessage {
                application_id: 8,
                line_instance: 2,
                call_reference: 43,
                transaction_id: 10,
                data: b"<CiscoIPPhoneResponse/>".to_vec(),
            }),
            ProtocolVersion::V17,
        );
        assert_client_round_trip(
            ClientMessage::DeviceToUserData(UserDataMessage {
                application_id: 9,
                line_instance: 2,
                call_reference: 44,
                transaction_id: 11,
                data: b"<CiscoIPPhoneInput/>".to_vec(),
            }),
            ProtocolVersion::V17,
        );
        assert_client_round_trip(
            ClientMessage::DeviceToUserDataResponseV1(UserDataV1Message {
                application_id: 9,
                line_instance: 2,
                call_reference: 44,
                transaction_id: 11,
                sequence_flag: 2,
                display_priority: 1,
                conference_id: 44,
                application_instance_id: 9,
                routing: 1,
                data: b"<CiscoIPPhoneResponse/>".to_vec(),
            }),
            ProtocolVersion::V17,
        );
        assert_client_round_trip(
            ClientMessage::LocationInfo {
                xml: "<location><building>west</building></location>".into(),
            },
            ProtocolVersion::V22,
        );
        assert_client_round_trip(
            ClientMessage::XmlAlarm(
                XmlAlarmMessage::from_xml(b"<alarm><severity>warning</severity></alarm>").unwrap(),
            ),
            ProtocolVersion::V22,
        );
        assert_client_round_trip(
            ClientMessage::CallCountRequest(CallCountRequestPayload::LegacyWord(2)),
            ProtocolVersion::V22,
        );
        assert_control_round_trip(
            ControlMessage::PortResponse(PortEndpoint {
                conference_id: 42,
                call_reference: 42,
                passthrough_party_id: 8,
                address: "2001:db8::8".parse().unwrap(),
                rtp_port: 16_000,
                rtcp_port: 16_001,
                media_type: Some(MediaType::Audio),
            }),
            ProtocolVersion::V22,
        );
        assert_control_round_trip(
            ControlMessage::CreateConferenceResponse(CreateConferenceResponse {
                conference_id: ConferenceId::new(42),
                result: CreateConferenceResult::Ok,
                passthrough_data: vec![1, 2, 3],
            }),
            ProtocolVersion::V22,
        );
        assert_control_round_trip(
            ControlMessage::DeleteConferenceResponse {
                conference_id: ConferenceId::new(42),
                result: DeleteConferenceResult::ConferenceDoesNotExist,
            },
            ProtocolVersion::V22,
        );
        assert_control_round_trip(
            ControlMessage::ModifyConferenceResponse(ModifyConferenceResponse {
                conference_id: ConferenceId::new(42),
                result: ModifyConferenceResult::MoreActiveCallsThanReserved,
                passthrough_data: vec![4, 5],
            }),
            ProtocolVersion::V22,
        );
        assert_control_round_trip(
            ControlMessage::AuditConferenceResponse(AuditConferenceResponse {
                last: 1,
                entries: vec![AuditConferenceEntry {
                    conference_id: ConferenceId::new(42),
                    resource_type: ConferenceResourceType::Conference,
                    reserved_participants: 8,
                    active_participants: 3,
                    application_id: ApplicationId::new(7),
                    application_conference_id: "festival-42".into(),
                    application_data: "main-stage".into(),
                }],
            }),
            ProtocolVersion::V22,
        );
        assert_control_round_trip(
            ControlMessage::AddParticipantResponse(AddParticipantResponse {
                conference_id: ConferenceId::new(42),
                call_reference: CallReference::new(100),
                result: AddParticipantResult::Ok,
                bridge_participant_id: BoundedBytes::try_from(vec![3; 257]).unwrap(),
            }),
            ProtocolVersion::V22,
        );
        assert_control_round_trip(
            ControlMessage::AuditParticipantResponse(AuditParticipantResponse {
                result: AuditParticipantResult::Ok,
                last: 1,
                conference_id: ConferenceId::new(42),
                number_of_entries: 2,
                participant_entries: vec![1, 2, 3, 4],
            }),
            ProtocolVersion::V22,
        );
    }

    #[test]
    fn common_server_messages_round_trip_semantically() {
        assert_server_round_trip(
            ServerMessage::SpeedDialStatus {
                instance: 7,
                number: "2001".into(),
                display_name: "Reception".into(),
            },
            ProtocolVersion::V3,
        );
        assert_server_round_trip(
            ServerMessage::ServiceUrlStatus {
                index: 4,
                url: "http://services.invalid/directory".into(),
                label: "Directory".into(),
                extension_text: String::new(),
            },
            ProtocolVersion::V3,
        );
        for protocol in [
            ProtocolVersion::V3,
            ProtocolVersion::V17,
            ProtocolVersion::V22,
        ] {
            assert_server_round_trip(
                ServerMessage::ConnectionStatisticsRequest {
                    directory_number: "1001".into(),
                    call_reference: 42,
                    processing: StatisticsProcessing::DoNotClear,
                },
                protocol,
            );
        }
        assert_server_round_trip(
            ServerMessage::DisplayPriorityNotify {
                timeout_seconds: 5,
                priority: NotificationPriority::Voicemail,
                text: "Incoming call".into(),
            },
            ProtocolVersion::V17,
        );
        assert_server_round_trip(
            ServerMessage::FeatureStatus {
                instance: 2,
                button_type: ButtonType::BlfSpeedDial,
                label: "Support".into(),
                state: 0x0002_0101,
            },
            ProtocolVersion::V22,
        );
        assert_server_round_trip(
            ServerMessage::PortRequest(PortRequest {
                conference_id: 42.into(),
                call_reference: 42.into(),
                passthrough_party_id: 9.into(),
                transport: MediaTransport::Rtp,
                address_type: Some(IpAddressType::Ipv4AndIpv6),
                media_type: Some(MediaType::Audio),
            }),
            ProtocolVersion::V22,
        );
        assert_server_round_trip(
            ServerMessage::Notification {
                transaction_id: 3,
                feature_id: 1,
                status: BusyLampFieldState::Unknown(77),
                text: "4000".into(),
            },
            ProtocolVersion::V22,
        );
        assert_server_round_trip(
            ServerMessage::SubscriptionStatus {
                transaction_id: 3,
                feature_id: 1,
                timer_seconds: 30,
                cause: SubscriptionCause::Ok,
            },
            ProtocolVersion::V22,
        );
        assert_server_round_trip(
            ServerMessage::UserToDeviceData(UserDataMessage {
                application_id: 7,
                line_instance: 1,
                call_reference: 42,
                transaction_id: 9,
                data: b"<CiscoIPPhoneText/>".to_vec(),
            }),
            ProtocolVersion::V17,
        );
        assert_server_round_trip(
            ServerMessage::UserToDeviceDataV1(UserDataV1Message {
                application_id: 7,
                line_instance: 1,
                call_reference: 42,
                transaction_id: 9,
                sequence_flag: 2,
                display_priority: 1,
                conference_id: 42,
                application_instance_id: 7,
                routing: 1,
                data: b"<CiscoIPPhoneMenu/>".to_vec(),
            }),
            ProtocolVersion::V17,
        );
        assert_server_round_trip(
            ServerMessage::CallHistoryDisposition {
                disposition: CallHistoryDisposition::Missed,
                line_instance: 1,
                call_reference: 42,
            },
            ProtocolVersion::V22,
        );
        assert_server_round_trip(
            ServerMessage::CallCountResponse(CallCountResponse {
                total_configured_lines: 2,
                starting_line_instance: 1,
                line_data: vec![
                    CallCountLineData {
                        max_calls: 4,
                        busy_trigger: 2,
                    },
                    CallCountLineData {
                        max_calls: 2,
                        busy_trigger: 1,
                    },
                ],
            }),
            ProtocolVersion::V22,
        );
        for message in [
            ServerMessage::SubscribeDtmfPayloadRequest(DtmfPayloadRequest {
                payload_type: 101,
                conference_id: 42,
                passthrough_party_id: 7,
                dtmf_type: 2,
            }),
            ServerMessage::SubscribeDtmfPayloadError(DtmfPayloadIdentity {
                payload_type: 102,
                conference_id: 43,
                passthrough_party_id: 8,
            }),
            ServerMessage::UnsubscribeDtmfPayloadRequest(DtmfPayloadRequest {
                payload_type: 103,
                conference_id: 44,
                passthrough_party_id: 9,
                dtmf_type: 3,
            }),
            ServerMessage::UnsubscribeDtmfPayloadError(DtmfPayloadIdentity {
                payload_type: 104,
                conference_id: 45,
                passthrough_party_id: 10,
            }),
        ] {
            let encoded = message.encode(ProtocolVersion::V22).unwrap();
            let frame = decode_frame(&encoded);
            assert!(matches!(frame.payload.len(), 12 | 16));
            assert_eq!(
                ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
                message
            );
        }
        assert_server_round_trip(
            ServerMessage::RecordingStatus {
                call_reference: 42,
                active: true,
            },
            ProtocolVersion::V22,
        );
        assert_control_round_trip(
            ControlMessage::StartAnnouncement {
                announcements: vec![
                    AnnouncementEntry {
                        locale: 1,
                        country: 46,
                        tone: Tone::Zip,
                    },
                    AnnouncementEntry {
                        locale: 0,
                        country: 0,
                        tone: Tone::Silence,
                    },
                    AnnouncementEntry {
                        locale: 2,
                        country: 1,
                        tone: Tone::RecorderWarning,
                    },
                ],
                end_of_ack: EndOfAnnouncementAck::Required,
                conference_id: 42,
                matrix_conference_party_ids: vec![7, 0, 9],
                hearing_conference_party_mask: 0b101,
                play_mode: AnnouncementPlayMode::Continuous,
            },
            ProtocolVersion::V22,
        );
        assert_control_round_trip(
            ControlMessage::StopAnnouncement { conference_id: 42 },
            ProtocolVersion::V22,
        );
        assert_control_round_trip(
            ControlMessage::AnnouncementFinish {
                conference_id: 42,
                play_status: AnnouncementPlayStatus::Unknown(3),
            },
            ProtocolVersion::V22,
        );
        assert_control_round_trip(
            ControlMessage::ClearConference {
                conference_id: ConferenceId::new(42),
                service_number: 3,
            },
            ProtocolVersion::V22,
        );
        assert_control_round_trip(
            ControlMessage::CreateConferenceRequest(CreateConferenceRequest {
                conference_id: ConferenceId::new(42),
                reserved_participants: 8,
                resource_type: ConferenceResourceType::Conference,
                application_id: ApplicationId::new(7),
                application_conference_id: "festival-42".into(),
                application_data: "main-stage".into(),
                passthrough_data: vec![1, 2, 3],
            }),
            ProtocolVersion::V22,
        );
        assert_control_round_trip(
            ControlMessage::DeleteConferenceRequest {
                conference_id: ConferenceId::new(42),
            },
            ProtocolVersion::V22,
        );
        assert_control_round_trip(
            ControlMessage::ModifyConferenceRequest(ModifyConferenceRequest {
                conference_id: ConferenceId::new(42),
                reserved_participants: 12,
                application_id: ApplicationId::new(7),
                application_conference_id: "festival-42".into(),
                application_data: "main-stage".into(),
                passthrough_data: vec![4, 5],
            }),
            ProtocolVersion::V22,
        );
        assert_control_round_trip(ControlMessage::AuditConferenceRequest, ProtocolVersion::V22);
        assert_control_round_trip(
            ControlMessage::AddParticipantRequest(AddParticipantRequest {
                conference_id: ConferenceId::new(42),
                participant: ConferenceParticipant {
                    call_reference: CallReference::new(100),
                    presentation_restrictions: PartyInformationRestrictions::CALLING_NUMBER,
                    name: "Festival Caller".into(),
                    number: "1001".into(),
                    conference_name: "Main Stage".into(),
                },
            }),
            ProtocolVersion::V22,
        );
        assert_control_round_trip(
            ControlMessage::DropParticipantRequest {
                conference_id: ConferenceId::new(42),
                call_reference: CallReference::new(100),
            },
            ProtocolVersion::V22,
        );
        assert_control_round_trip(
            ControlMessage::AuditParticipantRequest {
                conference_id: ConferenceId::new(42),
            },
            ProtocolVersion::V22,
        );
    }

    #[test]
    fn connection_statistics_round_trip_all_layouts_and_redact_opaque_fields() {
        let statistics = ConnectionStatistics {
            directory_number: "2002".into(),
            call_reference: 42,
            processing: StatisticsProcessing::Clear,
            packets_sent: 100,
            octets_sent: 8_000,
            packets_received: 98,
            octets_received: 7_840,
            packets_lost: 2,
            jitter_millis: 7,
            latency_millis: 18,
            quality: ConnectionQualityStatistics::new(b"MLQK=4.5;Secret=opaque".to_vec()).unwrap(),
        };
        for protocol in [
            ProtocolVersion::V3,
            ProtocolVersion::V19,
            ProtocolVersion::V22,
        ] {
            assert_client_round_trip(
                ClientMessage::ConnectionStatisticsResponse(statistics.clone()),
                protocol,
            );
        }
        let debug = format!("{statistics:?}");
        assert!(!debug.contains("2002"));
        assert!(!debug.contains("Secret"));
        assert!(debug.contains("byte_count"));
        assert!(matches!(
            ConnectionQualityStatistics::new(vec![0; CONNECTION_QUALITY_MAX_BYTES + 1]),
            Err(CodecError::CountTooLarge {
                field: "quality statistics",
                maximum: CONNECTION_QUALITY_MAX_BYTES,
                ..
            })
        ));
    }

    #[test]
    fn dtmf_subscription_messages_require_their_exact_word_layouts() {
        for message_id in [
            wire_id::SUBSCRIBE_DTMF_PAYLOAD_RES,
            wire_id::UNSUBSCRIBE_DTMF_PAYLOAD_RES,
        ] {
            assert!(ClientMessage::decode(Frame::new(22, message_id, Vec::new())).is_err());
            assert!(ClientMessage::decode(Frame::new(22, message_id, vec![0; 11])).is_err());
            assert!(ClientMessage::decode(Frame::new(22, message_id, vec![0; 12])).is_ok());
            assert!(ClientMessage::decode(Frame::new(22, message_id, vec![0; 13])).is_err());
        }
        for (message_id, size) in [
            (wire_id::SUBSCRIBE_DTMF_PAYLOAD_REQ, 16),
            (wire_id::SUBSCRIBE_DTMF_PAYLOAD_ERR, 12),
            (wire_id::UNSUBSCRIBE_DTMF_PAYLOAD_REQ, 16),
            (wire_id::UNSUBSCRIBE_DTMF_PAYLOAD_ERR, 12),
        ] {
            assert!(
                ServerMessage::decode(
                    Frame::new(22, message_id, vec![0; size - 1]),
                    ProtocolVersion::V22,
                )
                .is_err()
            );
            assert!(
                ServerMessage::decode(
                    Frame::new(22, message_id, vec![0; size]),
                    ProtocolVersion::V22,
                )
                .is_ok()
            );
            assert!(
                ServerMessage::decode(
                    Frame::new(22, message_id, vec![0; size + 1]),
                    ProtocolVersion::V22,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn announcement_lists_enforce_station_bounds() {
        let error = ServerMessage::StartAnnouncement {
            announcements: vec![
                AnnouncementEntry {
                    locale: 1,
                    country: 1,
                    tone: Tone::Zip,
                };
                33
            ],
            end_of_ack: 0,
            conference_id: 1,
            matrix_conference_party_ids: Vec::new(),
            hearing_conference_party_mask: 0,
            play_mode: 0,
        }
        .encode(ProtocolVersion::V22)
        .unwrap_err();
        assert!(matches!(
            error,
            CodecError::CountTooLarge {
                field: "announcements",
                count: 33,
                maximum: 32,
                ..
            }
        ));

        let error = ServerMessage::StartAnnouncement {
            announcements: Vec::new(),
            end_of_ack: 0,
            conference_id: 1,
            matrix_conference_party_ids: (1..=17).collect(),
            hearing_conference_party_mask: 0,
            play_mode: 0,
        }
        .encode(ProtocolVersion::V22)
        .unwrap_err();
        assert!(matches!(
            error,
            CodecError::CountTooLarge {
                field: "matrix conference party identifiers",
                count: 17,
                maximum: 16,
                ..
            }
        ));
    }

    #[test]
    fn enbloc_uses_the_protocol_19_text_width_boundary() {
        for (protocol, payload_len, line_offset) in [
            (ProtocolVersion::V18, 28, 24),
            (ProtocolVersion::V19, 32, 28),
        ] {
            let message = ClientMessage::EnblocCall {
                called_party: "9801".into(),
                line_instance: 3,
            };
            let frame = FrameDecoder::new()
                .push(&message.encode(protocol).unwrap())
                .unwrap()
                .remove(0);
            assert_eq!(frame.payload.len(), payload_len);
            assert_eq!(
                &frame.payload[line_offset..line_offset + 4],
                &3_u32.to_le_bytes()
            );
            assert_eq!(
                ClientMessage::decode_with_version(frame, protocol).unwrap(),
                message
            );
        }
    }

    #[test]
    fn supplemental_client_messages_have_typed_layouts() {
        let ports = ClientMessage::MediaPortList(MediaPortList {
            rtp_ports: vec![16_000, 16_002],
        });
        let frame = decode_frame(&ports.encode(ProtocolVersion::V22).unwrap());
        assert_eq!(frame.message_id, wire_id::MEDIA_PORT_LIST);
        assert_eq!(frame.payload.len(), 68);
        assert_eq!(
            &frame.payload[..12],
            &[2, 0, 0, 0, 0x80, 0x3e, 0, 0, 0x82, 0x3e, 0, 0]
        );
        assert_eq!(
            ClientMessage::decode_with_version(frame, ProtocolVersion::V22).unwrap(),
            ports
        );

        let token = ClientMessage::SpcpRegisterToken(SpcpRegisterTokenMessage {
            device_id: DeviceId::new("SEP001122334455").unwrap(),
            device_instance: 2,
            address: Ipv4Addr::new(192, 0, 2, 10),
            device_type: DeviceType::Cisco7962,
            max_streams: 0x0102_0304,
        });
        let frame = decode_frame(&token.encode(ProtocolVersion::V22).unwrap());
        assert_eq!(frame.message_id, wire_id::SPCP_REGISTER_TOKEN_REQ);
        assert_eq!(frame.payload.len(), 36);
        assert_eq!(&frame.payload[16..20], &[0; 4]);
        assert_eq!(&frame.payload[24..28], &[10, 2, 0, 192]);
        assert_eq!(&frame.payload[32..36], &[4, 3, 2, 1]);
        assert_eq!(
            ClientMessage::decode_with_version(frame, ProtocolVersion::V22).unwrap(),
            token
        );

        let oversized = ClientMessage::MediaPortList(MediaPortList {
            rtp_ports: vec![16_000; MEDIA_PORT_LIST_MAX_PORTS + 1],
        });
        assert!(matches!(
            oversized.encode(ProtocolVersion::V22),
            Err(CodecError::CountTooLarge { .. })
        ));

        let mut invalid_port = vec![0; 68];
        invalid_port[..4].copy_from_slice(&1_u32.to_le_bytes());
        invalid_port[4..8].copy_from_slice(&65_536_u32.to_le_bytes());
        assert!(matches!(
            ClientMessage::decode_with_version(
                Frame::new(22, wire_id::MEDIA_PORT_LIST, invalid_port),
                ProtocolVersion::V22,
            ),
            Err(CodecError::InvalidValue {
                field: "RTP port",
                ..
            })
        ));
    }

    #[test]
    fn supplemental_server_messages_have_typed_layouts() {
        for (message, id, payload) in [
            (
                ServerMessage::SetHookFlashDetect,
                wire_id::SET_HOOK_FLASH_DETECT,
                vec![],
            ),
            (
                ServerMessage::StartMediaReception,
                wire_id::START_MEDIA_RECEPTION,
                vec![],
            ),
            (
                ServerMessage::StopMediaReception {
                    conference_id: 0x0102_0304.into(),
                    passthrough_party_id: 0x0506_0708.into(),
                },
                wire_id::STOP_MEDIA_RECEPTION,
                vec![4, 3, 2, 1, 8, 7, 6, 5],
            ),
            (
                ServerMessage::EnunciatorCommand,
                wire_id::ENUNCIATOR_COMMAND,
                vec![],
            ),
            (
                ServerMessage::SpcpRegisterTokenAck {
                    features: 0x0102_0304,
                },
                wire_id::SPCP_REGISTER_TOKEN_ACK,
                vec![4, 3, 2, 1],
            ),
            (
                ServerMessage::SpcpRegisterTokenReject {
                    backoff_seconds: 60,
                },
                wire_id::SPCP_REGISTER_TOKEN_REJECT,
                vec![60, 0, 0, 0],
            ),
        ] {
            let frame = decode_frame(&message.encode(ProtocolVersion::V22).unwrap());
            assert_eq!(frame.message_id, id);
            assert_eq!(frame.payload, payload);
            assert_eq!(
                ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
                message
            );
        }

        assert!(
            ServerMessage::decode(
                Frame::new(22, wire_id::SET_HOOK_FLASH_DETECT, vec![0; 4]),
                ProtocolVersion::V22,
            )
            .is_err()
        );
    }

    #[test]
    fn unknown_messages_are_byte_lossless() {
        let unknown_payload = vec![9, 8, 7, 6];
        let unknown = ServerMessage::decode(
            Frame::new(19, 0xdead_beef, unknown_payload.clone()),
            ProtocolVersion::V19,
        )
        .unwrap();
        assert!(matches!(unknown, ServerMessage::Unknown(_)));
        let unknown_frame = decode_frame(&unknown.encode(ProtocolVersion::V22).unwrap());
        assert_eq!(unknown_frame.message_id, 0xdead_beef);
        assert_eq!(unknown_frame.protocol_version, 19);
        assert_eq!(unknown_frame.payload, unknown_payload);
    }

    #[test]
    fn opaque_encoding_cannot_bypass_a_typed_contract() {
        let message = ClientMessage::KnownOpaque(KnownOpaqueMessage {
            id: MessageId::IpPort,
            protocol_version: ProtocolVersion::V22.wire(),
            payload: BoundedBytes::default(),
        });

        assert!(matches!(
            message.encode(ProtocolVersion::V22),
            Err(CodecError::InvalidValue {
                message_id: wire_id::IP_PORT,
                field: "opaque preservation requires an opaque-only contract",
                ..
            })
        ));
    }

    #[test]
    fn malformed_counts_and_oversized_text_are_rejected() {
        let mut capabilities = vec![0; 4 + 18 * 16];
        capabilities[..4].copy_from_slice(&19_u32.to_le_bytes());
        assert!(matches!(
            ClientMessage::decode(Frame::new(22, wire_id::CAPABILITIES_RES, capabilities,)),
            Err(CodecError::CountTooLarge { .. })
        ));
        assert!(matches!(
            ServerMessage::DisplayText {
                text: "x".repeat(32),
            }
            .encode(ProtocolVersion::V22),
            Err(CodecError::TextTooLong { .. })
        ));
        assert!(matches!(
            ClientMessage::DeviceToUserData(UserDataMessage {
                application_id: 1,
                line_instance: 1,
                call_reference: 1,
                transaction_id: 1,
                data: vec![0; 2001],
            })
            .encode(ProtocolVersion::V22),
            Err(CodecError::CountTooLarge { .. })
        ));
        assert!(matches!(
            ClientMessage::decode(Frame::new(
                22,
                wire_id::IP_PORT,
                70_000_u32.to_le_bytes().to_vec(),
            )),
            Err(CodecError::InvalidValue { .. })
        ));
        assert!(matches!(
            ServerMessage::StartMediaTransmission {
                call_reference: 1,
                passthrough_party_id: 1,
                endpoint: MediaEndpoint {
                    address: "2001:db8::1".parse().unwrap(),
                    rtp_port: 4000,
                    rtcp_port: 4001,
                    codec: Codec::Pcmu,
                    packet_ms: 20,
                    max_frames_per_packet: 1,
                    telephone_event_payload: 101,
                },
                silence_suppression: SilenceSuppression::Off,
                traffic_class: crate::types::MediaTrafficClass::default(),
                encryption: None,
                wire: None,
            }
            .encode(ProtocolVersion::V3),
            Err(CodecError::InvalidValue { .. })
        ));
        assert!(matches!(
            ControlMessage::CreateConferenceRequest(CreateConferenceRequest {
                conference_id: ConferenceId::new(1),
                reserved_participants: 2,
                resource_type: ConferenceResourceType::Conference,
                application_id: ApplicationId::new(1),
                application_conference_id: "conference-1".into(),
                application_data: String::new(),
                passthrough_data: vec![0; 2001],
            })
            .encode(ProtocolVersion::V22),
            Err(CodecError::CountTooLarge {
                field: "conference passthrough data",
                count: 2001,
                maximum: 2000,
                ..
            })
        ));
        assert!(matches!(
            ControlMessage::AuditConferenceResponse(AuditConferenceResponse {
                last: 1,
                entries: vec![
                    AuditConferenceEntry {
                        conference_id: ConferenceId::new(1),
                        resource_type: ConferenceResourceType::Conference,
                        reserved_participants: 2,
                        active_participants: 1,
                        application_id: ApplicationId::new(1),
                        application_conference_id: String::new(),
                        application_data: String::new(),
                    };
                    33
                ],
            })
            .encode(ProtocolVersion::V22),
            Err(CodecError::CountTooLarge {
                field: "conference audit entries",
                count: 33,
                maximum: 32,
                ..
            })
        ));

        let mut oversized_conference_data = vec![0; 12];
        oversized_conference_data[8..12].copy_from_slice(&2001_u32.to_le_bytes());
        assert!(matches!(
            ControlMessage::decode(
                Frame::new(
                    22,
                    wire_id::CREATE_CONFERENCE_RES,
                    oversized_conference_data
                ),
                ProtocolVersion::V22,
            ),
            Err(CodecError::CountTooLarge {
                field: "conference passthrough data",
                count: 2001,
                maximum: 2000,
                ..
            })
        ));

        let mut oversized_audit = vec![0; 8];
        oversized_audit[4..8].copy_from_slice(&33_u32.to_le_bytes());
        assert!(matches!(
            ControlMessage::decode(
                Frame::new(22, wire_id::AUDIT_CONFERENCE_RES, oversized_audit),
                ProtocolVersion::V22,
            ),
            Err(CodecError::CountTooLarge {
                field: "conference audit entries",
                count: 33,
                maximum: 32,
                ..
            })
        ));
    }

    #[test]
    fn server_response_uses_the_negotiated_address_layout() {
        let message = ServerMessage::ServerResponse {
            servers: vec![
                SignalingServerEndpoint {
                    name: "primary".into(),
                    address: IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)),
                    port: NonZeroU16::new(2000).unwrap(),
                },
                SignalingServerEndpoint {
                    name: "secondary".into(),
                    address: IpAddr::V4(Ipv4Addr::new(192, 0, 2, 20)),
                    port: NonZeroU16::new(2001).unwrap(),
                },
            ],
        };
        let v3 = message.encode(ProtocolVersion::V3).unwrap();
        let v17 = message.encode(ProtocolVersion::V17).unwrap();
        assert_eq!(v3.len(), 292);
        assert_eq!(v17.len(), 372);
        assert_server_round_trip(message.clone(), ProtocolVersion::V3);
        assert_server_round_trip(message, ProtocolVersion::V17);

        let mut zero_port = v3;
        zero_port[12 + 5 * 48..12 + 5 * 48 + 4].fill(0);
        assert!(matches!(
            ServerMessage::decode(decode_frame(&zero_port), ProtocolVersion::V3),
            Err(CodecError::InvalidValue {
                field: "server endpoint",
                value: 0,
                ..
            })
        ));
        assert_server_round_trip(
            ServerMessage::ServerResponse {
                servers: vec![SignalingServerEndpoint {
                    name: "sccp-v6".into(),
                    address: "2001:db8::20".parse().unwrap(),
                    port: NonZeroU16::new(2000).unwrap(),
                }],
            },
            ProtocolVersion::V17,
        );

        let unspecified = ServerMessage::ServerResponse {
            servers: vec![SignalingServerEndpoint {
                name: "unroutable".into(),
                address: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                port: NonZeroU16::new(2000).unwrap(),
            }],
        };
        assert!(matches!(
            unspecified.encode(ProtocolVersion::V17),
            Err(CodecError::InvalidValue {
                field: "server address",
                value: 0,
                ..
            })
        ));

        let endpoints = |count: u8| {
            (0..count)
                .map(|index| SignalingServerEndpoint {
                    name: format!("node-{index}"),
                    address: IpAddr::V4(Ipv4Addr::new(192, 0, 2, index + 1)),
                    port: NonZeroU16::new(2000).unwrap(),
                })
                .collect()
        };
        let empty = ServerMessage::ServerResponse {
            servers: Vec::new(),
        };
        assert!(matches!(
            empty.encode(ProtocolVersion::V17),
            Err(CodecError::InvalidValue {
                field: "server endpoints",
                value: 0,
                ..
            })
        ));
        assert_server_round_trip(
            ServerMessage::ServerResponse {
                servers: endpoints(5),
            },
            ProtocolVersion::V17,
        );
        let too_many = ServerMessage::ServerResponse {
            servers: endpoints(6),
        };
        assert!(matches!(
            too_many.encode(ProtocolVersion::V17),
            Err(CodecError::CountTooLarge {
                field: "server endpoints",
                count: 6,
                maximum: 5,
                ..
            })
        ));
    }
}
