//! Private SCCP codec implementation and declarative payload layouts.
//!
//! Public message types describe protocol meaning. These types describe byte
//! layout only, which keeps reserved fields and version-specific structure out
//! of the application API. Some message identifiers support multiple body
//! sizes independently of the negotiated frame version.
//!
//! Decoder failures deliberately distinguish truncation, unsupported body
//! length, non-word-aligned station strings, non-zero/trailing padding, count
//! bounds, and invalid field values. Alternate layouts are selected by
//! protocol and/or exact body length so a typed decode does not silently turn
//! a valid frame into a different wire body.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::num::NonZeroU16;

use binrw::{BinRead, BinWrite};

use super::capabilities::{CapabilityUpdate, CapabilityUpdateVariant};
use super::catalog::{CodecSupport, MessageRoute};
use super::values::{
    AddParticipantResult, AlarmSeverity, AnnouncementPlayMode, AnnouncementPlayStatus,
    AuditParticipantResult, BusyLampFieldState, ButtonType, CallHistoryDisposition, CallState,
    Codec, ConferenceResourceType, CreateConferenceResult, DeleteConferenceResult, DeviceType,
    Digit, DynamicCallInfoLayout, EchoCancellation, EncryptionMethod, EndOfAnnouncementAck,
    G723BitRate, IpAddressType, KeyMode, LampMode, MediaPathCapability, MediaPathEvent,
    MediaPathId, MediaStatus, MediaTransport, MediaType, MessageWaitingResult, MicrophoneMode,
    ModifyConferenceResult, NotificationPriority, PartyInformationRestrictions, PhoneFeatures,
    ProtocolVersion, QosDirection, QosErrorCode, QosReservationStyle, ResetType, RingDuration,
    RingerMode, RsvpErrorCode, SilenceSuppression, SoftKey, SpeakerMode, StationSessionContext,
    StatisticsProcessing, Stimulus, SubscriptionCause, Tone, ToneDirection,
};
use super::wire::{CodecError, Frame};
use super::*;
use crate::types::{
    CallInfo, DateTemplate, DeviceId, LegacyCodePage, MAX_STATION_BUTTON_INSTANCE, MediaEndpoint,
    MediaTrafficClass, SoftKeyProfile,
};

mod conference;
mod fixed_text;
mod io;
mod media;
mod qos;
mod services;
mod station;
use conference::*;
use fixed_text::{WireFixedText, station_text_bytes};
use io::{
    decode, decode_prefix, decode_zero_padded, encode, usize_from_wire, validate_exact_payload,
    validate_payload_bounds, validate_zero_payload, wire_count,
};
use media::*;
use qos::*;
use services::*;
use station::*;

fn ensure_station_route(
    frame: &Frame,
    expected: MessageRoute,
    expected_name: &'static str,
) -> Result<(), CodecError> {
    let Some(actual) = frame.message_type().route() else {
        return Ok(());
    };
    if actual == expected {
        Ok(())
    } else {
        Err(CodecError::UnexpectedRoute {
            message_id: frame.message_id,
            actual,
            expected: expected_name,
        })
    }
}

fn validate_media_port_count(message_id: u32, count: usize) -> Result<(), CodecError> {
    match count {
        0..=MEDIA_PORT_LIST_MAX_PORTS => Ok(()),
        _ => Err(CodecError::CountTooLarge {
            message_id,
            field: "RTP ports",
            count,
            maximum: MEDIA_PORT_LIST_MAX_PORTS,
        }),
    }
}

fn preserve_known_message(frame: Frame, id: MessageId) -> Result<KnownOpaqueMessage, CodecError> {
    ensure_preserve_only(id)?;
    let payload = BoundedBytes::try_from(frame.payload).map_err(|error| {
        CodecError::FrameTooLarge(error.actual.saturating_add(super::wire::HEADER_SIZE))
    })?;
    Ok(KnownOpaqueMessage {
        id,
        protocol_version: frame.protocol_version,
        payload,
    })
}

fn ensure_preserve_only(id: MessageId) -> Result<(), CodecError> {
    if id
        .contract()
        .is_some_and(|contract| contract.codec == CodecSupport::OpaqueOnly)
    {
        Ok(())
    } else {
        Err(CodecError::InvalidValue {
            message_id: id.wire_value(),
            field: "opaque preservation requires an opaque-only contract",
            value: u64::from(id.wire_value()),
        })
    }
}

fn pad_typed_payload(message_id: u32, payload: &mut Vec<u8>) {
    use super::catalog::PayloadLayout;

    let Some(contract) = MessageId::from(message_id).contract() else {
        return;
    };
    if !matches!(
        contract.payload_layout,
        PayloadLayout::Opaque
            | PayloadLayout::BoundedOpaque
            | PayloadLayout::BoundedPreserved
            | PayloadLayout::VersionAndLengthSelected
            | PayloadLayout::MinimumLengthPreserved
    ) {
        pad_dynamic_payload(payload);
    }
}

fn canonical_open_receive_wire(
    call_reference: u32,
    source_address: IpAddr,
    protocol: ProtocolVersion,
) -> OpenReceiveChannelWire {
    OpenReceiveChannelWire {
        conference_id: call_reference,
        g723_bitrate: 0,
        stream_passthrough_id: 0,
        associated_stream_id: 0,
        dtmf_type: 10,
        mixing_mode: 0,
        direction: u32::from(protocol.wire() >= 12),
        requested_address_type: u32::from(
            protocol.wire() >= 17 && matches!(source_address, IpAddr::V6(_)),
        ),
        audio_level_adjustment: 0,
        latent_capabilities: [0; 36],
    }
}

fn canonical_start_media_wire(
    call_reference: u32,
    protocol: ProtocolVersion,
) -> StartMediaTransmissionWire {
    StartMediaTransmissionWire {
        conference_id: call_reference,
        g723_bitrate: 0,
        stream_passthrough_id: 0,
        associated_stream_id: 0,
        dtmf_type: 10,
        mixing_mode: 0,
        direction: u32::from(protocol.wire() >= 12),
        latent_capabilities: [0; 36],
    }
}

#[derive(BinRead, BinWrite, Clone, Copy, Default, Eq, PartialEq)]
#[brw(little)]
struct WireEncryptionInfo {
    algorithm: u32,
    key_length: u16,
    salt_length: u16,
    key: [u8; 16],
    salt: [u8; 16],
    mki_present: u32,
    key_derivation_rate: u32,
}

impl std::fmt::Debug for WireEncryptionInfo {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WireEncryptionInfo")
            .field("algorithm", &EncryptionMethod::from(self.algorithm))
            .field("key", &"<redacted>")
            .field("key_length", &self.key_length)
            .field("salt", &"<redacted>")
            .field("salt_length", &self.salt_length)
            .field("mki_present", &self.mki_present)
            .field("key_derivation_rate", &self.key_derivation_rate)
            .finish()
    }
}

impl WireEncryptionInfo {
    fn from_public(encryption: Option<&MediaEncryption>) -> Self {
        let Some(encryption) = encryption else {
            return Self::default();
        };
        Self {
            algorithm: encryption.algorithm.wire_value(),
            key_length: u16::from(encryption.key_length),
            salt_length: u16::from(encryption.salt_length),
            key: encryption.key,
            salt: encryption.salt,
            mki_present: encryption.mki_present,
            key_derivation_rate: encryption.key_derivation_rate,
        }
    }

    fn to_public(self, _message_id: u32) -> Result<Option<MediaEncryption>, CodecError> {
        if usize::from(self.key_length) > self.key.len() {
            return Err(CodecError::SecretTooLong {
                field: "media encryption key",
                actual: usize::from(self.key_length),
                maximum: self.key.len(),
            });
        }
        if usize::from(self.salt_length) > self.salt.len() {
            return Err(CodecError::SecretTooLong {
                field: "media encryption salt",
                actual: usize::from(self.salt_length),
                maximum: self.salt.len(),
            });
        }
        if self.algorithm == 0
            && self.key_length == 0
            && self.salt_length == 0
            && self.mki_present == 0
            && self.key_derivation_rate == 0
        {
            return Ok(None);
        }
        Ok(Some(MediaEncryption::from_wire(
            EncryptionMethod::from(self.algorithm),
            self.key,
            self.key_length as u8,
            self.salt,
            self.salt_length as u8,
            self.mki_present,
            self.key_derivation_rate,
        )))
    }
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireLatentCapabilities {
    bytes: [u8; 36],
}

impl Default for WireLatentCapabilities {
    fn default() -> Self {
        Self { bytes: [0; 36] }
    }
}

trait WireIpAddress:
    for<'a> BinRead<Args<'a> = ()>
    + for<'a> BinWrite<Args<'a> = ()>
    + Clone
    + Copy
    + std::fmt::Debug
    + Eq
    + PartialEq
    + 'static
{
    fn from_ip(address: IpAddr, message_id: u32, field: &'static str) -> Result<Self, CodecError>;
    fn to_ip(self, message_id: u32) -> Result<IpAddr, CodecError>;
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
struct WireIpv4Address {
    bytes: [u8; 4],
}

impl From<[u8; 4]> for WireIpv4Address {
    fn from(bytes: [u8; 4]) -> Self {
        Self { bytes }
    }
}

impl From<Ipv4Addr> for WireIpv4Address {
    fn from(address: Ipv4Addr) -> Self {
        address.octets().into()
    }
}

impl From<WireIpv4Address> for Ipv4Addr {
    fn from(address: WireIpv4Address) -> Self {
        Self::from(address.bytes)
    }
}

impl WireIpAddress for WireIpv4Address {
    fn from_ip(address: IpAddr, message_id: u32, field: &'static str) -> Result<Self, CodecError> {
        let IpAddr::V4(address) = address else {
            return Err(CodecError::InvalidValue {
                message_id,
                field,
                value: 1,
            });
        };
        Ok(Self {
            bytes: address.octets(),
        })
    }

    fn to_ip(self, _message_id: u32) -> Result<IpAddr, CodecError> {
        Ok(IpAddr::V4(Ipv4Addr::from(self.bytes)))
    }
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireExtendedAddress {
    family: u32,
    bytes: [u8; 16],
}

impl WireExtendedAddress {
    fn from_ip(address: IpAddr) -> Self {
        match address {
            IpAddr::V4(address) => {
                let mut bytes = [0; 16];
                bytes[..4].copy_from_slice(&address.octets());
                Self { family: 0, bytes }
            }
            IpAddr::V6(address) => Self {
                family: 1,
                bytes: address.octets(),
            },
        }
    }

    fn to_ip(self, message_id: u32) -> Result<IpAddr, CodecError> {
        match self.family {
            0 => Ok(IpAddr::V4(Ipv4Addr::new(
                self.bytes[0],
                self.bytes[1],
                self.bytes[2],
                self.bytes[3],
            ))),
            1 => Ok(IpAddr::V6(Ipv6Addr::from(self.bytes))),
            value => Err(CodecError::InvalidValue {
                message_id,
                field: "IP address family",
                value: u64::from(value),
            }),
        }
    }
}

impl WireIpAddress for WireExtendedAddress {
    fn from_ip(
        address: IpAddr,
        _message_id: u32,
        _field: &'static str,
    ) -> Result<Self, CodecError> {
        Ok(Self::from_ip(address))
    }

    fn to_ip(self, message_id: u32) -> Result<IpAddr, CodecError> {
        self.to_ip(message_id)
    }
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireStartMulticastReception<Address: WireIpAddress> {
    conference_id: u32,
    passthrough_party_id: u32,
    address: Address,
    port: u32,
    packet_millis: u32,
    codec: u32,
    echo_cancellation: u32,
    g723_bitrate: u32,
    call_reference: u32,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireStartMulticastTransmission<Address: WireIpAddress> {
    conference_id: u32,
    passthrough_party_id: u32,
    address: Address,
    port: u32,
    packet_millis: u32,
    codec: u32,
    precedence: u32,
    silence_suppression: u32,
    max_frames_per_packet: u32,
    g723_bitrate: u32,
    call_reference: u32,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireOpenReceiveV11 {
    conference_id: u32,
    passthrough_party_id: u32,
    packet_millis: u32,
    codec: u32,
    vad: u32,
    g723_bitrate: u32,
    call_reference: u32,
    encryption: WireEncryptionInfo,
    stream_passthrough_id: u32,
    associated_stream_id: u32,
    rfc2833_payload: u32,
    dtmf_type: u32,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireOpenReceiveAddressed<Address: WireIpAddress> {
    base: WireOpenReceiveV11,
    mixing_mode: u32,
    direction: u32,
    remote: Address,
    remote_port: u32,
}

type WireOpenReceiveV12 = WireOpenReceiveAddressed<WireIpv4Address>;

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireOpenReceiveV17 {
    base: WireOpenReceiveAddressed<WireExtendedAddress>,
    requested_address_type: u32,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireOpenReceiveV18 {
    base: WireOpenReceiveV17,
    audio_level_adjustment: u32,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireOpenReceiveV21 {
    base: WireOpenReceiveV18,
    latent_capabilities: WireLatentCapabilities,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireStartMediaBase<Address: WireIpAddress> {
    conference_id: u32,
    passthrough_party_id: u32,
    remote: Address,
    remote_port: u32,
    packet_millis: u32,
    codec: u32,
    precedence: u32,
    silence_suppression: u32,
    max_frames_per_packet: u32,
    g723_bitrate: u32,
    call_reference: u32,
    encryption: WireEncryptionInfo,
    stream_passthrough_id: u32,
    associated_stream_id: u32,
    rfc2833_payload: u32,
    dtmf_type: u32,
}

type WireStartMediaV11 = WireStartMediaBase<WireIpv4Address>;

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireStartMediaDirected<Address: WireIpAddress> {
    base: WireStartMediaBase<Address>,
    mixing_mode: u32,
    direction: u32,
}

type WireStartMediaV12 = WireStartMediaDirected<WireIpv4Address>;
type WireStartMediaV17 = WireStartMediaDirected<WireExtendedAddress>;

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireStartMediaV21 {
    base: WireStartMediaV17,
    latent_capabilities: WireLatentCapabilities,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireStartMediaAck<Address: WireIpAddress> {
    conference_id: u32,
    passthrough_party_id: u32,
    call_reference: u32,
    address: Address,
    port: u32,
    status: u32,
}

type WireStartMediaAckV3 = WireStartMediaAck<WireIpv4Address>;
type WireStartMediaAckV17 = WireStartMediaAck<WireExtendedAddress>;

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireStartMediaAckV20 {
    base: WireStartMediaAckV17,
    extension: [u8; 8],
}

macro_rules! words {
    ($name:ident { $($field:ident),+ $(,)? }) => {
        #[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
        #[brw(little)]
        struct $name {
            $($field: u32),+
        }
    };
}

words!(WireOneWord { value });
words!(WireMulticastReceptionAck {
    status,
    passthrough_party_id,
    call_reference
});
words!(WireLineCall {
    line_instance,
    call_reference
});
words!(WireCallParty {
    call_reference,
    passthrough_party_id
});
words!(WireAudioStreamControl {
    conference_id,
    passthrough_party_id,
    call_reference,
    port_handling_flag
});
words!(WireSelectSoftKeys {
    line_instance,
    call_reference,
    set,
    valid_mask
});
words!(WireCallState {
    state,
    line_instance,
    call_reference,
    visibility,
    precedence,
    domain
});
words!(WireCallInfoDynamicHeader {
    line_instance,
    call_reference,
    call_type,
    original_redirect_reason,
    last_redirect_reason,
    call_instance,
    security_status,
    party_restrictions
});
words!(WireDynamicPromptHeader {
    timeout_seconds,
    line_instance,
    call_reference
});
words!(WireModeLineCall {
    mode,
    duration,
    line_instance,
    call_reference
});
words!(WireToneLineCall {
    tone,
    direction,
    line_instance,
    call_reference
});
words!(WireLampState {
    stimulus,
    instance,
    mode
});
words!(WirePortRequest {
    conference_id,
    call_reference,
    passthrough_party_id,
    transport
});
#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WirePortRequestV20 {
    base: WirePortRequest,
    address_type: u32,
    media_type: u32,
}
words!(WirePortClose {
    conference_id,
    call_reference,
    passthrough_party_id
});
#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WirePortCloseV20 {
    base: WirePortClose,
    media_type: u32,
}
words!(WireSubscriptionStatus {
    transaction_id,
    feature_id,
    timer_seconds,
    cause
});
words!(WireCallSelectStatus {
    status,
    call_reference,
    line_instance
});
words!(WireRecordingStatus {
    call_reference,
    active
});
words!(WireFeatureStatusRequest {
    index,
    capabilities
});
words!(WireLineStatusDynamicHeader {
    line_instance,
    line_type
});
words!(WireStopToneV12 {
    line_instance,
    call_reference,
    tone
});
words!(WireCallHistoryDisposition {
    disposition,
    line_instance,
    call_reference
});
words!(WireAnnouncementFinish {
    conference_id,
    play_status
});
words!(WireStopMulticast {
    conference_id,
    passthrough_party_id,
    call_reference
});
words!(WireAddParticipantResponseHeader {
    conference_id,
    call_reference,
    result
});
words!(WireAuditParticipantResponseHeader {
    result,
    last,
    conference_id,
    number_of_entries
});

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Default, Eq, PartialEq)]
#[brw(little)]
struct WireAnnouncementEntry {
    locale: u32,
    country: u32,
    tone: u32,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireStartAnnouncement {
    announcements: [WireAnnouncementEntry; 32],
    end_of_ack: u32,
    conference_id: u32,
    matrix_conference_party_ids: [u32; 16],
    hearing_conference_party_mask: u32,
    play_mode: u32,
}

#[derive(BinRead, BinWrite, Clone, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireCreateConferenceRequest {
    conference_id: u32,
    reserved_participants: u32,
    resource_type: u32,
    application_id: u32,
    application_conference_id: WireFixedText<32>,
    application_data: WireFixedText<24>,
    data_length: u32,
    #[br(count = data_length)]
    passthrough_data: Vec<u8>,
}

#[derive(BinRead, BinWrite, Clone, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireModifyConferenceRequest {
    conference_id: u32,
    reserved_participants: u32,
    application_id: u32,
    application_conference_id: WireFixedText<32>,
    application_data: WireFixedText<24>,
    data_length: u32,
    #[br(count = data_length)]
    passthrough_data: Vec<u8>,
}

#[derive(BinRead, BinWrite, Clone, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireConferenceResponse {
    conference_id: u32,
    result: u32,
    data_length: u32,
    #[br(count = data_length)]
    passthrough_data: Vec<u8>,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireAuditConferenceEntry {
    conference_id: u32,
    resource_type: u32,
    reserved_participants: u32,
    active_participants: u32,
    application_id: u32,
    application_conference_id: WireFixedText<32>,
    application_data: WireFixedText<24>,
}

#[derive(BinRead, BinWrite, Clone, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireAuditConferenceResponse {
    last: u32,
    number_of_entries: u32,
    #[br(count = number_of_entries)]
    entries: Vec<WireAuditConferenceEntry>,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireParticipantRequest {
    conference_id: u32,
    call_reference: u32,
    presentation_restrictions: u32,
    participant_name: WireFixedText<40>,
    participant_number: WireFixedText<24>,
    conference_name: WireFixedText<32>,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireQosFlow {
    conference_id: u32,
    call_reference: u32,
    passthrough_party_id: u32,
    address: [u8; 4],
    port: u32,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireQosApplicationIdentifier {
    vendor_id: WireFixedText<32>,
    version: WireFixedText<16>,
    application_name: WireFixedText<32>,
    sub_application_id: WireFixedText<32>,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireQosReservationNotify {
    flow: WireQosFlow,
    direction: u32,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireUpdateDscp {
    flow: WireQosFlow,
    dscp: u32,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireQosErrorNotify {
    flow: WireQosFlow,
    direction: u32,
    error_code: u32,
    failure_node: u32,
    rsvp_error_code: u32,
    rsvp_error_subcode: u32,
    rsvp_error_flags: u32,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireQosListen {
    flow: WireQosFlow,
    reservation_style: u32,
    maximum_retries: u32,
    retry_timer: u32,
    confirmation_required: u32,
    preemption_priority: u32,
    defending_priority: u32,
    compression_type: u32,
    average_bit_rate: u32,
    burst_size: u32,
    peak_rate: u32,
    application: WireQosApplicationIdentifier,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireQosPath {
    flow: WireQosFlow,
    reservation_style: u32,
    maximum_retries: u32,
    retry_timer: u32,
    preemption_priority: u32,
    defending_priority: u32,
    compression_type: u32,
    average_bit_rate: u32,
    burst_size: u32,
    peak_rate: u32,
    application: WireQosApplicationIdentifier,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireQosModify {
    flow: WireQosFlow,
    direction: u32,
    compression_type: u32,
    average_bit_rate: u32,
    burst_size: u32,
    peak_rate: u32,
    application: WireQosApplicationIdentifier,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireMessageWaitingNotification {
    target_number: WireFixedText<25>,
    control_number: WireFixedText<25>,
    alignment: [u8; 2],
    messages_waiting: u32,
    total_voicemail_new: u32,
    total_voicemail_old: u32,
    priority_voicemail_new: u32,
    priority_voicemail_old: u32,
    total_fax_new: u32,
    total_fax_old: u32,
    priority_fax_new: u32,
    priority_fax_old: u32,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireMessageWaitingResponse {
    target_number: WireFixedText<25>,
    alignment: [u8; 3],
    result: u32,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireRegisterAck {
    keepalive_seconds: u32,
    date_template: [u8; 6],
    alignment: [u8; 2],
    secondary_keepalive_seconds: u32,
    protocol_features: [u8; 4],
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireConfigStatus {
    device_id: WireFixedText<16>,
    station_user_id: u32,
    station_instance: u32,
    user_name: WireFixedText<40>,
    server_name: WireFixedText<40>,
    line_count: u32,
    speed_dial_count: u32,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireLineStatus {
    line_instance: u32,
    directory_number: WireFixedText<24>,
    display_name: WireFixedText<40>,
    display_label: WireFixedText<40>,
    reserved: u32,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Default, Eq, PartialEq)]
struct WireButtonDefinition {
    instance: u8,
    button_type: u8,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireButtonTemplate {
    offset: u32,
    count: u32,
    total: u32,
    definitions: [WireButtonDefinition; BUTTON_TEMPLATE_ENTRIES_PER_CHUNK],
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireServerResponse<Address: WireIpAddress> {
    names: [WireFixedText<48>; 5],
    ports: [u32; 5],
    addresses: [Address; 5],
}

words!(WireTimeDate {
    year,
    month,
    weekday,
    day,
    hour,
    minute,
    second,
    milliseconds,
    unix_seconds
});

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireSoftKeyDefinition {
    label: [u8; 16],
    event: u32,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireSoftKeyTemplate {
    offset: u32,
    count: u32,
    total: u32,
    definitions: [WireSoftKeyDefinition; 32],
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Default, Eq, PartialEq)]
#[brw(little)]
struct WireSoftKeySetDefinition {
    template_indexes: [u8; 16],
    info: [u16; 16],
}

#[derive(BinRead, BinWrite, Clone, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireSoftKeySet {
    offset: u32,
    count: u32,
    total: u32,
    #[br(count = 16)]
    sets: Vec<WireSoftKeySetDefinition>,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireCallInfo {
    calling_name: WireFixedText<40>,
    calling_number: WireFixedText<24>,
    called_name: WireFixedText<40>,
    called_number: WireFixedText<24>,
    line_instance: u32,
    call_reference: u32,
    call_type: u32,
    original_called_name: WireFixedText<40>,
    original_called_number: WireFixedText<24>,
    last_redirecting_name: WireFixedText<40>,
    last_redirecting_number: WireFixedText<24>,
    original_redirect_reason: u32,
    last_redirect_reason: u32,
    voice_mailboxes: [WireFixedText<24>; 4],
    call_instance: u32,
    security_status: u32,
    party_restrictions: u32,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WirePromptStatus {
    timeout_seconds: u32,
    text: WireFixedText<32>,
    line_instance: u32,
    call_reference: u32,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireNotify {
    timeout_seconds: u32,
    text: WireFixedText<32>,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireDynamicNotifyHeader {
    timeout_seconds: u32,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WirePriorityNotify {
    timeout_seconds: u32,
    priority: u32,
    text: WireFixedText<32>,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireDynamicPriorityNotifyHeader {
    timeout_seconds: u32,
    priority: u32,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
struct WireAlignedText<const TEXT_BYTES: usize, const ALIGNMENT_BYTES: usize> {
    value: WireFixedText<TEXT_BYTES>,
    alignment: [u8; ALIGNMENT_BYTES],
}

impl<const TEXT_BYTES: usize, const ALIGNMENT_BYTES: usize>
    WireAlignedText<TEXT_BYTES, ALIGNMENT_BYTES>
{
    fn new(message_id: u32, field: &'static str, value: &str) -> Result<Self, CodecError> {
        Ok(Self {
            value: WireFixedText::new(message_id, field, value)?,
            alignment: [0; ALIGNMENT_BYTES],
        })
    }

    fn text(&self) -> Result<String, CodecError> {
        self.value.text()
    }

    fn validate(&self, message_id: u32) -> Result<(), CodecError> {
        validate_zero_payload(&self.alignment, message_id, ALIGNMENT_BYTES)
    }
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireConnectionStatisticsRequest<const TEXT_BYTES: usize, const ALIGNMENT_BYTES: usize> {
    directory_number: WireAlignedText<TEXT_BYTES, ALIGNMENT_BYTES>,
    call_reference: u32,
    processing: u32,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireForwardTarget<const TEXT_BYTES: usize, const ALIGNMENT_BYTES: usize> {
    active: u32,
    number: WireAlignedText<TEXT_BYTES, ALIGNMENT_BYTES>,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireForwardStatus<const TEXT_BYTES: usize, const ALIGNMENT_BYTES: usize> {
    active: u32,
    line_instance: u32,
    all: WireForwardTarget<TEXT_BYTES, ALIGNMENT_BYTES>,
    busy: WireForwardTarget<TEXT_BYTES, ALIGNMENT_BYTES>,
    no_answer: WireForwardTarget<TEXT_BYTES, ALIGNMENT_BYTES>,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireSpeedDialStatus {
    instance: u32,
    number: WireFixedText<24>,
    display_name: WireFixedText<40>,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireDialedNumber<const TEXT_BYTES: usize, const ALIGNMENT_BYTES: usize> {
    number: WireAlignedText<TEXT_BYTES, ALIGNMENT_BYTES>,
    line_instance: u32,
    call_reference: u32,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireFeatureStatus {
    instance: u32,
    button_type: u32,
    label: WireFixedText<40>,
    state: u32,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireFeatureStatusDynamic {
    instance: u32,
    button_type: u32,
    state: u32,
    label: WireFixedText<121>,
    padding: [u8; 3],
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireServiceUrlStatus {
    index: u32,
    url: WireFixedText<256>,
    label: WireFixedText<40>,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireNotification {
    transaction_id: u32,
    feature_id: u32,
    status: u32,
    text: WireFixedText<100>,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireRegister {
    device_id: WireFixedText<16>,
    station_user_id: u32,
    station_instance: u32,
    reported_address: [u8; 4],
    device_type: u32,
    max_streams: u32,
    active_streams: u32,
    protocol_features: [u8; 4],
    max_conferences: u32,
    active_conferences: u32,
    mac_address: [u8; 12],
    ipv4_address_scope: u32,
    max_lines: u32,
    ipv6_address: [u8; 16],
    ipv6_address_scope: u32,
    firmware: WireFixedText<32>,
}

const REGISTER_ALTERNATE_BYTES: usize = 32;
const REGISTER_CANONICAL_BYTES: usize = 124;
const REGISTER_MAXIMUM_BYTES: usize = 172;

const fn valid_registration_canonical_prefix(bytes: usize) -> bool {
    matches!(bytes, 36 | 40 | 44 | 48 | 52 | 64 | 68 | 72 | 88 | 92 | 124)
}

fn registration_layout(
    message_id: u32,
    payload_bytes: usize,
) -> Result<RegistrationWireLayout, CodecError> {
    if payload_bytes < REGISTER_ALTERNATE_BYTES {
        return Err(CodecError::Truncated {
            message_id,
            needed: REGISTER_ALTERNATE_BYTES,
            actual: payload_bytes,
        });
    }
    if payload_bytes > REGISTER_MAXIMUM_BYTES {
        return Err(CodecError::TrailingBytes {
            message_id,
            count: payload_bytes - REGISTER_MAXIMUM_BYTES,
        });
    }
    if payload_bytes == REGISTER_ALTERNATE_BYTES {
        return Ok(RegistrationWireLayout::Alternate32);
    }

    let prefix_bytes = payload_bytes.min(REGISTER_CANONICAL_BYTES);
    if !valid_registration_canonical_prefix(prefix_bytes) {
        return Err(CodecError::InvalidValue {
            message_id,
            field: "registration payload length",
            value: payload_bytes as u64,
        });
    }
    Ok(RegistrationWireLayout::Canonical {
        prefix_bytes: u8::try_from(prefix_bytes)
            .expect("registration prefix is bounded to 124 bytes"),
    })
}

fn registration_prefix_bytes(layout: RegistrationWireLayout) -> Result<usize, CodecError> {
    match layout {
        RegistrationWireLayout::Alternate32 => Ok(REGISTER_ALTERNATE_BYTES),
        RegistrationWireLayout::Canonical { prefix_bytes } => {
            let prefix_bytes = usize::from(prefix_bytes);
            if valid_registration_canonical_prefix(prefix_bytes) {
                Ok(prefix_bytes)
            } else {
                Err(CodecError::InvalidValue {
                    message_id: wire_id::REGISTER,
                    field: "canonical registration prefix length",
                    value: prefix_bytes as u64,
                })
            }
        }
    }
}

fn validate_registration_fields(
    registration: &RegistrationMessage,
    wire: RegistrationWireDetails,
    prefix_bytes: usize,
) -> Result<(), CodecError> {
    let alternate = wire.layout == RegistrationWireLayout::Alternate32;
    let canonical_prefix = (!alternate).then_some(prefix_bytes);
    let omits = |field_end| canonical_prefix.is_none_or(|bytes| bytes < field_end);
    let incompatible_field = [
        (
            alternate && registration.reported_address.is_some(),
            "reported IPv4 address",
        ),
        (alternate && !registration.features.is_empty(), "features"),
        (omits(36) && wire.max_streams != 0, "maximum streams"),
        (omits(40) && wire.active_streams != 0, "active streams"),
        (
            omits(48) && wire.max_conferences != 0,
            "maximum conferences",
        ),
        (
            omits(52) && wire.active_conferences != 0,
            "active conferences",
        ),
        (
            omits(64) && wire.mac_address_and_padding.iter().any(|byte| *byte != 0),
            "MAC address and padding",
        ),
        (
            omits(68) && wire.ipv4_address_scope != 0,
            "IPv4 address scope",
        ),
        (omits(72) && wire.max_lines != 0, "maximum lines"),
        (
            omits(88) && registration.reported_ipv6_address.is_some(),
            "reported IPv6 address",
        ),
        (
            omits(92) && wire.ipv6_address_scope != 0,
            "IPv6 address scope",
        ),
        (omits(124) && !registration.firmware.is_empty(), "firmware"),
        (
            omits(124) && !registration.configuration_version_stamp.is_empty(),
            "configuration version stamp",
        ),
    ]
    .into_iter()
    .find_map(|(incompatible, field)| incompatible.then_some(field));
    if let Some(field) = incompatible_field {
        return Err(CodecError::InvalidValue {
            message_id: wire_id::REGISTER,
            field,
            value: prefix_bytes as u64,
        });
    }
    if !alternate
        && prefix_bytes < 44
        && (registration.advertised_protocol.is_some() || !registration.features.is_empty())
    {
        return Err(CodecError::InvalidValue {
            message_id: wire_id::REGISTER,
            field: "protocol fields require a 44-byte registration prefix",
            value: prefix_bytes as u64,
        });
    }
    if !alternate && prefix_bytes >= 52 && registration.advertised_protocol.is_none() {
        return Err(CodecError::InvalidValue {
            message_id: wire_id::REGISTER,
            field: "registration protocol is absent from a layout that carries it",
            value: prefix_bytes as u64,
        });
    }
    Ok(())
}

words!(WireKeypadButtonLegacy { button });

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireKeypadButtonWithCall {
    base: WireKeypadButtonLegacy,
    line_instance: u32,
    call_reference: u32,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireKeypadButton {
    base: WireKeypadButtonWithCall,
    keypad_union: u32,
    reserved: u32,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireEnblocWithoutLine {
    called_party: WireFixedText<24>,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireEnblocWithLine<const TEXT_BYTES: usize, const ALIGNMENT_BYTES: usize> {
    called_party: WireAlignedText<TEXT_BYTES, ALIGNMENT_BYTES>,
    line_instance: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EnblocWireLayout {
    WithoutLine,
    Line24,
    Line25Packed,
    Line25Aligned,
    Line25Candidates,
}

impl EnblocWireLayout {
    fn select(protocol: u32, payload_bytes: usize, message_id: u32) -> Result<Self, CodecError> {
        match (protocol, payload_bytes) {
            (..=16, 24) => Ok(Self::WithoutLine),
            (..=18, 28) => Ok(Self::Line24),
            (19.., 29..=31) => Ok(Self::Line25Packed),
            (19.., 32) => Ok(Self::Line25Candidates),
            _ => Err(CodecError::InvalidLength(message_id)),
        }
    }

    const fn canonical(protocol: u32) -> Self {
        match protocol {
            ..=16 => Self::WithoutLine,
            17..=18 => Self::Line24,
            19.. => Self::Line25Aligned,
        }
    }
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireOffHookWithCallingParty<const TEXT_BYTES: usize, const ALIGNMENT_BYTES: usize> {
    calling_party_number: WireFixedText<TEXT_BYTES>,
    voice_mailbox: WireFixedText<TEXT_BYTES>,
    alignment: [u8; ALIGNMENT_BYTES],
    line_instance: u32,
}

words!(WireStimulus {
    stimulus,
    instance,
    call_reference,
    status
});

const CAPABILITIES_RESPONSE_STANDARD_ENTRIES: usize = 18;
const CAPABILITIES_RESPONSE_EXTENDED_ENTRIES: usize = 24;
const CAPABILITIES_RESPONSE_MAX_ENTRIES: usize = CAPABILITIES_RESPONSE_EXTENDED_ENTRIES;

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Default, Eq, PartialEq)]
#[brw(little)]
struct WireMediaCapability {
    codec: u32,
    max_frames_per_packet: u32,
    codec_parameters: [u8; 8],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CapabilitiesResponseWireLayout {
    Standard,
    Extended,
}

impl CapabilitiesResponseWireLayout {
    const fn entries(self) -> usize {
        match self {
            Self::Standard => CAPABILITIES_RESPONSE_STANDARD_ENTRIES,
            Self::Extended => CAPABILITIES_RESPONSE_EXTENDED_ENTRIES,
        }
    }

    const fn payload_bytes(self) -> usize {
        4 + self.entries() * 16
    }
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Default, Eq, PartialEq)]
#[brw(little)]
struct WireCallCountLineData {
    max_calls: u16,
    busy_trigger: u16,
}

#[derive(BinRead, BinWrite, Clone, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireCallCountResponse {
    total_configured_lines: u32,
    starting_line_instance: u32,
    line_data_entries: u32,
    line_data: [WireCallCountLineData; CALL_COUNT_RESPONSE_MAX_LINE_ENTRIES],
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireMediaPortList {
    count: u32,
    ports: [u32; MEDIA_PORT_LIST_MAX_PORTS],
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireAlarmBase {
    severity: u32,
    text: WireFixedText<80>,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireAlarm {
    base: WireAlarmBase,
    parameter_1: u32,
    parameter_2: u32,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireLocationInfo {
    xml: WireFixedText<2401>,
    alignment: [u8; 3],
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireReceiveChannelAck<Address: WireIpAddress> {
    status: u32,
    address: Address,
    port: u32,
    passthrough_party_id: u32,
    call_reference: u32,
}

type WireOpenReceiveAckV3 = WireReceiveChannelAck<WireIpv4Address>;
type WireOpenReceiveAckV17 = WireReceiveChannelAck<WireExtendedAddress>;

words!(WireSoftKeyEvent {
    event,
    line_instance,
    call_reference
});

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireRegisterToken {
    device_id: WireFixedText<16>,
    device_instance: u32,
    ipv4_address: [u8; 4],
    device_type: u32,
    ipv6_address: [u8; 16],
    flags: u32,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireSpcpRegisterToken {
    device_id: WireFixedText<16>,
    reserved: u32,
    device_instance: u32,
    ipv4_address: u32,
    device_type: u32,
    max_streams: u32,
}

words!(WireStopMediaReception {
    conference_id,
    passthrough_party_id
});

words!(WireMediaResourceNotification {
    device_type,
    in_service_streams,
    max_streams_per_conference,
    out_of_service_streams
});
words!(WireAccessoryStatus { accessory, state });
words!(WireDtmfToneControl {
    tone,
    conference_id,
    passthrough_party_id
});
words!(WireDtmfPayloadIdentity {
    payload_type,
    conference_id,
    passthrough_party_id
});
words!(WireDtmfPayloadRequest {
    payload_type,
    conference_id,
    passthrough_party_id,
    dtmf_type
});
#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireMediaFailureDetection {
    conference_id: u32,
    passthrough_party_id: u32,
    packet_millis: u32,
    codec: u32,
    echo_cancellation: u32,
    codec_qualifier: [u8; 4],
    call_reference: u32,
}
words!(WireMultimediaStreamControl {
    conference_id,
    passthrough_party_id,
    call_reference,
    port_handling_flag
});
words!(WireVideoFlowControl {
    conference_id,
    passthrough_party_id,
    call_reference,
    maximum_bit_rate
});
words!(WireVideoDisplayCommand {
    conference_id,
    call_reference,
    layout_id
});

type WireOpenMultimediaAckPre17 = WireReceiveChannelAck<WireIpv4Address>;
type WireOpenMultimediaAckFrom17 = WireReceiveChannelAck<WireExtendedAddress>;
type WireStartMultimediaAckPre17 = WireStartMediaAck<WireIpv4Address>;
type WireStartMultimediaAckFrom17 = WireStartMediaAck<WireExtendedAddress>;

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireSessionTransmission<Address: WireIpAddress> {
    remote_address: Address,
    session_type: u32,
}

type WireSessionTransmissionPre17 = WireSessionTransmission<WireIpv4Address>;
type WireSessionTransmissionFrom17 = WireSessionTransmission<WireExtendedAddress>;

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireMultimediaPayloadDescriptor {
    payload_rfc_number: u32,
    payload_type: u32,
}

impl From<MultimediaPayloadDescriptor> for WireMultimediaPayloadDescriptor {
    fn from(value: MultimediaPayloadDescriptor) -> Self {
        Self {
            payload_rfc_number: value.rfc_number(),
            payload_type: value.payload_number().into(),
        }
    }
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireOpenMultimediaV11 {
    conference_id: u32,
    passthrough_party_id: u32,
    compression_type: u32,
    line_instance: u32,
    call_reference: u32,
    payload_type: WireMultimediaPayloadDescriptor,
    conference_creator: u32,
    capability: [u8; MULTIMEDIA_CAPABILITY_BYTES],
    encryption: WireEncryptionInfo,
    stream_passthrough_id: u32,
    associated_stream_id: u32,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireOpenMultimediaAddressed<Address: WireIpAddress> {
    base: WireOpenMultimediaV11,
    source_address: Address,
    source_port: u32,
}

type WireOpenMultimediaV12 = WireOpenMultimediaAddressed<WireIpv4Address>;

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireOpenMultimediaV17 {
    base: WireOpenMultimediaAddressed<WireExtendedAddress>,
    requested_address_type: u32,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireStartMultimedia<Address: WireIpAddress> {
    conference_id: u32,
    passthrough_party_id: u32,
    compression_type: u32,
    remote_address: Address,
    remote_port: u32,
    call_reference: u32,
    payload_type: WireMultimediaPayloadDescriptor,
    dscp: u32,
    capability: [u8; MULTIMEDIA_CAPABILITY_BYTES],
    encryption: WireEncryptionInfo,
    stream_passthrough_id: u32,
    associated_stream_id: u32,
}

type WireStartMultimediaPre17 = WireStartMultimedia<WireIpv4Address>;
type WireStartMultimediaFrom17 = WireStartMultimedia<WireExtendedAddress>;

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireMiscellaneousCommand {
    conference_id: u32,
    passthrough_party_id: u32,
    call_reference: u32,
    command: u32,
    data: [u8; 36],
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireExtensionDeviceCapabilities {
    unknown_1: u32,
    unknown_2: u32,
    unknown_3: u32,
    description: WireFixedText<152>,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireMediaFailure<Address: WireIpAddress> {
    conference_id: u32,
    passthrough_party_id: u32,
    address: Address,
    port: u32,
    call_reference: u32,
}

type WireMediaFailureV3 = WireMediaFailure<WireIpv4Address>;
type WireMediaFailureV17 = WireMediaFailure<WireExtendedAddress>;

#[derive(BinRead, BinWrite, Clone, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireUserDataHeader {
    application_id: u32,
    line_instance: u32,
    call_reference: u32,
    transaction_id: u32,
    data_length: u32,
}

#[derive(BinRead, BinWrite, Clone, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireUserData {
    header: WireUserDataHeader,
    #[br(count = header.data_length)]
    data: Vec<u8>,
}

#[derive(BinRead, BinWrite, Clone, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireUserDataV1 {
    header: WireUserDataHeader,
    sequence_flag: u32,
    display_priority: u32,
    conference_id: u32,
    application_instance_id: u32,
    routing: u32,
    #[br(count = header.data_length)]
    data: Vec<u8>,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WirePortResponse<Address: WireIpAddress> {
    conference_id: u32,
    call_reference: u32,
    passthrough_party_id: u32,
    address: Address,
    rtp_port: u32,
    rtcp_port: u32,
}

type WirePortResponseV3 = WirePortResponse<WireIpv4Address>;

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WirePortResponseV20 {
    base: WirePortResponse<WireExtendedAddress>,
    media_type: u32,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireSubscriptionRequest {
    transaction_id: u32,
    feature_id: u32,
    timer_seconds: u32,
    subscription_id: WireFixedText<256>,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireConnectionStatisticsCounters {
    packets_sent: u32,
    octets_sent: u32,
    packets_received: u32,
    octets_received: u32,
    packets_lost: u32,
    jitter_millis: u32,
    latency_millis: u32,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireConnectionStatisticsTail {
    counters: WireConnectionStatisticsCounters,
    quality_size: u32,
}

trait WireStatisticsProcessing:
    for<'a> BinRead<Args<'a> = ()>
    + for<'a> BinWrite<Args<'a> = ()>
    + Clone
    + Copy
    + std::fmt::Debug
    + Eq
    + PartialEq
    + 'static
{
    fn to_wire(self) -> u32;
}

impl WireStatisticsProcessing for u32 {
    fn to_wire(self) -> u32 {
        self
    }
}

impl WireStatisticsProcessing for u8 {
    fn to_wire(self) -> u32 {
        u32::from(self)
    }
}

#[derive(BinRead, BinWrite, Clone, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireConnectionStatistics<
    const TEXT_BYTES: usize,
    const ALIGNMENT_BYTES: usize,
    Processing: WireStatisticsProcessing,
> {
    directory_number: WireAlignedText<TEXT_BYTES, ALIGNMENT_BYTES>,
    call_reference: u32,
    processing: Processing,
    statistics: WireConnectionStatisticsTail,
    #[br(count = statistics.quality_size)]
    quality: Vec<u8>,
}

type WireConnectionStatisticsV3 = WireConnectionStatistics<24, 0, u32>;
type WireConnectionStatisticsV19 = WireConnectionStatistics<25, 3, u32>;

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireConnectionStatisticsPrefix<
    const TEXT_BYTES: usize,
    const ALIGNMENT_BYTES: usize,
    Processing: WireStatisticsProcessing,
> {
    directory_number: WireAlignedText<TEXT_BYTES, ALIGNMENT_BYTES>,
    call_reference: u32,
    processing: Processing,
    statistics: WireConnectionStatisticsTail,
}

type WireConnectionStatisticsV3Prefix = WireConnectionStatisticsPrefix<24, 0, u32>;
type WireConnectionStatisticsV19Prefix = WireConnectionStatisticsPrefix<25, 3, u32>;

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireConnectionStatisticsPackedBase {
    directory_number: WireAlignedText<28, 0>,
    call_reference: u32,
    processing: u8,
    counters: WireConnectionStatisticsCounters,
}

#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireConnectionStatisticsPackedPrefix {
    base: WireConnectionStatisticsPackedBase,
    quality_size: u32,
}

fn validate_enbloc_line_instance(line_instance: u32, message_id: u32) -> Result<(), CodecError> {
    if line_instance <= MAX_STATION_BUTTON_INSTANCE {
        Ok(())
    } else {
        Err(CodecError::InvalidValue {
            message_id,
            field: "line instance",
            value: u64::from(line_instance),
        })
    }
}

fn enbloc_from_wire<const TEXT_BYTES: usize, const ALIGNMENT_BYTES: usize>(
    payload: &[u8],
    message_id: u32,
) -> Result<ClientMessage, CodecError> {
    let value: WireEnblocWithLine<TEXT_BYTES, ALIGNMENT_BYTES> = decode(message_id, payload)?;
    value.called_party.validate(message_id)?;
    validate_enbloc_line_instance(value.line_instance, message_id)?;
    Ok(ClientMessage::EnblocCall {
        called_party: value.called_party.text()?,
        line_instance: value.line_instance,
    })
}

fn enbloc_from_packed_wire(payload: &[u8], message_id: u32) -> Result<ClientMessage, CodecError> {
    let value: WireEnblocWithLine<25, 0> = decode_prefix(message_id, payload)?;
    value.called_party.validate(message_id)?;
    validate_zero_payload(&payload[29..], message_id, payload.len() - 29)?;
    validate_enbloc_line_instance(value.line_instance, message_id)?;
    Ok(ClientMessage::EnblocCall {
        called_party: value.called_party.text()?,
        line_instance: value.line_instance,
    })
}

fn select_enbloc_candidate(
    packed: Result<ClientMessage, CodecError>,
    aligned: Result<ClientMessage, CodecError>,
    payload_bytes: usize,
    message_id: u32,
) -> Result<ClientMessage, CodecError> {
    match (packed, aligned) {
        (Ok(packed), Ok(aligned)) if packed == aligned => Ok(packed),
        (Ok(_), Ok(_)) => Err(CodecError::InvalidValue {
            message_id,
            field: "conflicting Enbloc layout",
            value: payload_bytes as u64,
        }),
        (Ok(message), Err(_)) | (Err(_), Ok(message)) => Ok(message),
        (Err(error), Err(_)) => Err(error),
    }
}

fn decode_enbloc(
    payload: &[u8],
    protocol: u32,
    message_id: u32,
) -> Result<ClientMessage, CodecError> {
    match EnblocWireLayout::select(protocol, payload.len(), message_id)? {
        EnblocWireLayout::WithoutLine => {
            let value: WireEnblocWithoutLine = decode(message_id, payload)?;
            Ok(ClientMessage::EnblocCall {
                called_party: value.called_party.text()?,
                line_instance: 0,
            })
        }
        EnblocWireLayout::Line24 => enbloc_from_wire::<24, 0>(payload, message_id),
        EnblocWireLayout::Line25Packed => enbloc_from_packed_wire(payload, message_id),
        EnblocWireLayout::Line25Aligned => enbloc_from_wire::<25, 3>(payload, message_id),
        EnblocWireLayout::Line25Candidates => select_enbloc_candidate(
            enbloc_from_packed_wire(payload, message_id),
            enbloc_from_wire::<25, 3>(payload, message_id),
            payload.len(),
            message_id,
        ),
    }
}

fn enbloc_to_wire<const TEXT_BYTES: usize, const ALIGNMENT_BYTES: usize>(
    called_party: &str,
    line_instance: u32,
) -> Result<Vec<u8>, CodecError> {
    validate_enbloc_line_instance(line_instance, wire_id::ENBLOC_CALL)?;
    encode(
        wire_id::ENBLOC_CALL,
        &WireEnblocWithLine::<TEXT_BYTES, ALIGNMENT_BYTES> {
            called_party: WireAlignedText::new(wire_id::ENBLOC_CALL, "called party", called_party)?,
            line_instance,
        },
    )
}

fn encode_enbloc(
    called_party: &str,
    line_instance: u32,
    protocol: u32,
) -> Result<Vec<u8>, CodecError> {
    match EnblocWireLayout::canonical(protocol) {
        EnblocWireLayout::WithoutLine if line_instance == 0 => encode(
            wire_id::ENBLOC_CALL,
            &WireEnblocWithoutLine {
                called_party: WireFixedText::new(
                    wire_id::ENBLOC_CALL,
                    "called party",
                    called_party,
                )?,
            },
        ),
        EnblocWireLayout::WithoutLine => Err(CodecError::InvalidValue {
            message_id: wire_id::ENBLOC_CALL,
            field: "line instance is unavailable before protocol 17",
            value: u64::from(line_instance),
        }),
        EnblocWireLayout::Line24 => enbloc_to_wire::<24, 0>(called_party, line_instance),
        EnblocWireLayout::Line25Packed => enbloc_to_wire::<25, 0>(called_party, line_instance),
        EnblocWireLayout::Line25Aligned => enbloc_to_wire::<25, 3>(called_party, line_instance),
        EnblocWireLayout::Line25Candidates => unreachable!("candidate selection is decode-only"),
    }
}

fn decode_on_hook(payload: &[u8], message_id: u32) -> Result<ClientMessage, CodecError> {
    match payload.len() {
        0 => Ok(ClientMessage::OnHook {
            line_instance: 0,
            call_reference: 0,
        }),
        8 => {
            let value: WireLineCall = decode(message_id, payload)?;
            Ok(ClientMessage::OnHook {
                line_instance: value.line_instance,
                call_reference: value.call_reference,
            })
        }
        _ => Err(CodecError::InvalidLength(message_id)),
    }
}

fn decode_capabilities_response(
    payload: &[u8],
    message_id: u32,
) -> Result<ClientMessage, CodecError> {
    if payload.len() < 4 {
        return Err(CodecError::Truncated {
            message_id,
            needed: 4,
            actual: payload.len(),
        });
    }
    let count = usize_from_wire(
        message_id,
        "audio capabilities",
        decode_prefix::<WireOneWord>(message_id, payload)?.value,
    )?;
    if count > CAPABILITIES_RESPONSE_MAX_ENTRIES {
        return Err(CodecError::CountTooLarge {
            message_id,
            field: "audio capabilities",
            count,
            maximum: CAPABILITIES_RESPONSE_MAX_ENTRIES,
        });
    }
    let counted_bytes = 4 + count * 16;
    let reservoir_entries = match payload.len() {
        bytes if bytes == counted_bytes => None,
        bytes if bytes == CapabilitiesResponseWireLayout::Standard.payload_bytes() => {
            Some(CapabilitiesResponseWireLayout::Standard.entries())
        }
        bytes if bytes == CapabilitiesResponseWireLayout::Extended.payload_bytes() => {
            Some(CapabilitiesResponseWireLayout::Extended.entries())
        }
        actual if actual < counted_bytes => {
            return Err(CodecError::Truncated {
                message_id,
                needed: counted_bytes,
                actual,
            });
        }
        actual => {
            return Err(CodecError::TrailingBytes {
                message_id,
                count: actual - counted_bytes,
            });
        }
    };
    if let Some(entries) = reservoir_entries
        && count > entries
    {
        return Err(CodecError::CountTooLarge {
            message_id,
            field: "audio capabilities",
            count,
            maximum: entries,
        });
    }

    let (entries, remainder) = payload[4..].as_chunks::<16>();
    if !remainder.is_empty() {
        return Err(CodecError::InvalidLength(message_id));
    }
    let mut capabilities = Vec::with_capacity(count);
    for entry in entries.iter().take(count) {
        let capability: WireMediaCapability = decode(message_id, entry)?;
        capabilities.push(MediaCapability {
            codec: Codec::from(capability.codec),
            max_packet_ms: capability.max_frames_per_packet,
            codec_parameters: capability.codec_parameters,
        });
    }
    Ok(ClientMessage::CapabilitiesResponse(capabilities))
}

fn encode_capabilities_response(capabilities: &[MediaCapability]) -> Result<Vec<u8>, CodecError> {
    let layout = match capabilities.len() {
        0..=CAPABILITIES_RESPONSE_STANDARD_ENTRIES => CapabilitiesResponseWireLayout::Standard,
        19..=CAPABILITIES_RESPONSE_EXTENDED_ENTRIES => CapabilitiesResponseWireLayout::Extended,
        count => {
            return Err(CodecError::CountTooLarge {
                message_id: wire_id::CAPABILITIES_RES,
                field: "audio capabilities",
                count,
                maximum: CAPABILITIES_RESPONSE_MAX_ENTRIES,
            });
        }
    };
    let mut payload = encode(
        wire_id::CAPABILITIES_RES,
        &WireOneWord {
            value: wire_count(
                wire_id::CAPABILITIES_RES,
                "audio capabilities",
                capabilities.len(),
            )?,
        },
    )?;
    for capability in capabilities {
        payload.extend_from_slice(&encode(
            wire_id::CAPABILITIES_RES,
            &WireMediaCapability {
                codec: capability.codec.wire_value(),
                max_frames_per_packet: capability.max_packet_ms,
                codec_parameters: capability.codec_parameters,
            },
        )?);
    }
    payload.resize(layout.payload_bytes(), 0);
    Ok(payload)
}

fn decode_off_hook_with_calling_party<const TEXT_BYTES: usize, const ALIGNMENT_BYTES: usize>(
    payload: &[u8],
    message_id: u32,
) -> Result<ClientMessage, CodecError> {
    let value: WireOffHookWithCallingParty<TEXT_BYTES, ALIGNMENT_BYTES> =
        decode(message_id, payload)?;
    validate_zero_payload(&value.alignment, message_id, ALIGNMENT_BYTES)?;
    Ok(ClientMessage::OffHookWithCallingParty {
        calling_party_number: value.calling_party_number.text()?,
        voice_mailbox: value.voice_mailbox.text()?,
        line_instance: value.line_instance,
    })
}

fn encode_off_hook_with_calling_party<const TEXT_BYTES: usize, const ALIGNMENT_BYTES: usize>(
    calling_party_number: &str,
    voice_mailbox: &str,
    line_instance: u32,
) -> Result<Vec<u8>, CodecError> {
    encode(
        wire_id::OFF_HOOK_WITH_CALLING_PARTY,
        &WireOffHookWithCallingParty::<TEXT_BYTES, ALIGNMENT_BYTES> {
            calling_party_number: WireFixedText::new(
                wire_id::OFF_HOOK_WITH_CALLING_PARTY,
                "calling party number",
                calling_party_number,
            )?,
            voice_mailbox: WireFixedText::new(
                wire_id::OFF_HOOK_WITH_CALLING_PARTY,
                "voice mailbox",
                voice_mailbox,
            )?,
            alignment: [0; ALIGNMENT_BYTES],
            line_instance,
        },
    )
}

fn decode_connection_statistics_request<const TEXT_BYTES: usize, const ALIGNMENT_BYTES: usize>(
    payload: &[u8],
    message_id: u32,
) -> Result<ServerMessage, CodecError> {
    let value: WireConnectionStatisticsRequest<TEXT_BYTES, ALIGNMENT_BYTES> =
        decode(message_id, payload)?;
    value.directory_number.validate(message_id)?;
    Ok(ServerMessage::ConnectionStatisticsRequest {
        directory_number: value.directory_number.text()?,
        call_reference: value.call_reference,
        processing: StatisticsProcessing::from(value.processing),
    })
}

fn encode_connection_statistics_request<const TEXT_BYTES: usize, const ALIGNMENT_BYTES: usize>(
    directory_number: &str,
    call_reference: u32,
    processing: StatisticsProcessing,
) -> Result<Vec<u8>, CodecError> {
    encode(
        wire_id::CONNECTION_STATISTICS_REQ,
        &WireConnectionStatisticsRequest::<TEXT_BYTES, ALIGNMENT_BYTES> {
            directory_number: WireAlignedText::new(
                wire_id::CONNECTION_STATISTICS_REQ,
                "directory number",
                directory_number,
            )?,
            call_reference,
            processing: processing.wire_value(),
        },
    )
}

fn decode_forward_status<const TEXT_BYTES: usize, const ALIGNMENT_BYTES: usize>(
    payload: &[u8],
    message_id: u32,
) -> Result<ServerMessage, CodecError> {
    let value: WireForwardStatus<TEXT_BYTES, ALIGNMENT_BYTES> =
        decode_zero_padded(message_id, payload)?;
    value.all.number.validate(message_id)?;
    value.busy.number.validate(message_id)?;
    value.no_answer.number.validate(message_id)?;
    Ok(ServerMessage::ForwardStatus {
        forward_all: (value.all.active != 0)
            .then(|| value.all.number.text())
            .transpose()?,
        forward_busy: (value.busy.active != 0)
            .then(|| value.busy.number.text())
            .transpose()?,
        forward_no_answer: (value.no_answer.active != 0)
            .then(|| value.no_answer.number.text())
            .transpose()?,
        line_instance: value.line_instance,
    })
}

fn encode_forward_status<const TEXT_BYTES: usize, const ALIGNMENT_BYTES: usize>(
    line_instance: u32,
    forward_all: Option<&str>,
    forward_busy: Option<&str>,
    forward_no_answer: Option<&str>,
) -> Result<Vec<u8>, CodecError> {
    let target = |value: Option<&str>| -> Result<_, CodecError> {
        Ok(WireForwardTarget {
            active: u32::from(value.is_some()),
            number: WireAlignedText::new(
                wire_id::FORWARD_STAT,
                "forward number",
                value.unwrap_or(""),
            )?,
        })
    };
    encode(
        wire_id::FORWARD_STAT,
        &WireForwardStatus::<TEXT_BYTES, ALIGNMENT_BYTES> {
            active: u32::from(
                forward_all.is_some() || forward_busy.is_some() || forward_no_answer.is_some(),
            ),
            line_instance,
            all: target(forward_all)?,
            busy: target(forward_busy)?,
            no_answer: target(forward_no_answer)?,
        },
    )
}

fn decode_dialed_number<const TEXT_BYTES: usize, const ALIGNMENT_BYTES: usize>(
    payload: &[u8],
    message_id: u32,
) -> Result<ServerMessage, CodecError> {
    let value: WireDialedNumber<TEXT_BYTES, ALIGNMENT_BYTES> =
        decode_zero_padded(message_id, payload)?;
    value.number.validate(message_id)?;
    Ok(ServerMessage::DialedNumber {
        number: value.number.text()?,
        line_instance: value.line_instance,
        call_reference: value.call_reference,
    })
}

fn encode_dialed_number<const TEXT_BYTES: usize, const ALIGNMENT_BYTES: usize>(
    number: &str,
    line_instance: u32,
    call_reference: u32,
) -> Result<Vec<u8>, CodecError> {
    encode(
        wire_id::DIALED_NUMBER,
        &WireDialedNumber::<TEXT_BYTES, ALIGNMENT_BYTES> {
            number: WireAlignedText::new(wire_id::DIALED_NUMBER, "dialed number", number)?,
            line_instance,
            call_reference,
        },
    )
}

impl ClientMessage {
    /// Decodes a station-originated frame with an explicit negotiated version.
    ///
    /// Use this after registration when the session version is authoritative,
    /// especially for layouts whose header version is zero or ambiguous.
    pub fn decode_with_version(
        frame: Frame,
        protocol: ProtocolVersion,
    ) -> Result<Self, CodecError> {
        ensure_station_route(&frame, MessageRoute::StationToControl, "station-to-control")?;
        Self::decode_using_protocol(frame, protocol.wire())
    }

    /// Decodes a station-originated frame using its header version.
    ///
    /// This is suitable for initial messages that carry a meaningful header
    /// version. Established sessions should prefer [`Self::decode_with_version`].
    pub fn decode(frame: Frame) -> Result<Self, CodecError> {
        ensure_station_route(&frame, MessageRoute::StationToControl, "station-to-control")?;
        let protocol_version = frame.protocol_version;
        Self::decode_using_protocol(frame, protocol_version)
    }

    fn decode_using_protocol(frame: Frame, protocol_version: u32) -> Result<Self, CodecError> {
        let p = &frame.payload;
        match frame.message_id {
            wire_id::KEEP_ALIVE => Ok(Self::KeepAlive),
            wire_id::REGISTER => {
                let layout = registration_layout(frame.message_id, p.len())?;
                let prefix_bytes = registration_prefix_bytes(layout)?;
                let mut canonical = [0; REGISTER_CANONICAL_BYTES];
                canonical[..prefix_bytes].copy_from_slice(&p[..prefix_bytes]);
                let value: WireRegister = decode(frame.message_id, &canonical)?;
                let alternate = layout == RegistrationWireLayout::Alternate32;
                if alternate && value.reported_address[1..].iter().any(|byte| *byte != 0) {
                    return Err(CodecError::NonZeroPadding {
                        message_id: frame.message_id,
                        field: "alternate registration protocol",
                    });
                }
                let reported_address = (!alternate)
                    .then(|| Ipv4Addr::from(value.reported_address))
                    .filter(|address| !address.is_unspecified());
                let reported_ipv6_address = if !alternate
                    && prefix_bytes >= 88
                    && value.ipv6_address.iter().any(|byte| *byte != 0)
                {
                    Some(Ipv6Addr::from(value.ipv6_address))
                } else {
                    None
                };
                let advertised_protocol = if alternate {
                    Some(u32::from(value.reported_address[0])).filter(|version| *version != 0)
                } else if prefix_bytes >= 44 {
                    Some(u32::from(value.protocol_features[0]))
                        .filter(|version| *version != 0 || !matches!(prefix_bytes, 44 | 48))
                } else {
                    None
                };
                Ok(Self::Register(RegistrationMessage {
                    device_id: DeviceId::new(value.device_id.text()?)?,
                    reported_address,
                    reported_ipv6_address,
                    device_type: DeviceType::from(value.device_type),
                    advertised_protocol,
                    features: if !alternate && prefix_bytes >= 44 {
                        PhoneFeatures::from_bits_retain(
                            u32::from_le_bytes(value.protocol_features) & !0xff,
                        )
                    } else {
                        PhoneFeatures::empty()
                    },
                    firmware: if !alternate && prefix_bytes >= REGISTER_CANONICAL_BYTES {
                        value.firmware.text()?
                    } else {
                        String::new()
                    },
                    configuration_version_stamp: BoundedBytes::try_from(
                        p[REGISTER_CANONICAL_BYTES.min(p.len())..].to_vec(),
                    )
                    .expect("registration suffix length was bounded before allocation"),
                    wire: Some(RegistrationWireDetails {
                        layout,
                        station_user_id: value.station_user_id,
                        station_instance: value.station_instance,
                        max_streams: value.max_streams,
                        active_streams: value.active_streams,
                        mac_address_and_padding: value.mac_address,
                        max_conferences: value.max_conferences,
                        active_conferences: value.active_conferences,
                        ipv4_address_scope: value.ipv4_address_scope,
                        max_lines: value.max_lines,
                        ipv6_address_scope: value.ipv6_address_scope,
                    }),
                }))
            }
            wire_id::IP_PORT => {
                let value: WireOneWord = decode(frame.message_id, p)?;
                Ok(Self::IpPort {
                    rtp_port: decode_port(value.value, frame.message_id, "RTP port")?,
                })
            }
            wire_id::KEYPAD_BUTTON => {
                let (button, line_instance, call_reference, wire_layout) = match p.len() {
                    4 => {
                        let value: WireKeypadButtonLegacy = decode(frame.message_id, p)?;
                        (
                            value.button,
                            0,
                            0,
                            Some(KeypadButtonWireLayout::LegacyButtonOnly),
                        )
                    }
                    12 => {
                        let value: WireKeypadButtonWithCall = decode(frame.message_id, p)?;
                        (
                            value.base.button,
                            value.line_instance,
                            value.call_reference,
                            Some(KeypadButtonWireLayout::WithCallIdentity),
                        )
                    }
                    20 => {
                        let value: WireKeypadButton = decode(frame.message_id, p)?;
                        if value.keypad_union != 0 || value.reserved != 0 {
                            return Err(CodecError::InvalidValue {
                                message_id: frame.message_id,
                                field: "keypad reserved fields",
                                value: 1,
                            });
                        }
                        (
                            value.base.base.button,
                            value.base.line_instance,
                            value.base.call_reference,
                            None,
                        )
                    }
                    _ => return Err(CodecError::InvalidLength(frame.message_id)),
                };
                Ok(Self::KeypadButton {
                    button: Digit::from_keypad(button),
                    line_instance,
                    call_reference,
                    wire_layout,
                })
            }
            wire_id::ENBLOC_CALL => decode_enbloc(p, protocol_version, frame.message_id),
            wire_id::STIMULUS => {
                let value: WireStimulus = decode(frame.message_id, p)?;
                Ok(Self::Stimulus {
                    stimulus: Stimulus::from(value.stimulus),
                    instance: value.instance,
                    call_reference: value.call_reference,
                    status: value.status,
                })
            }
            wire_id::OFF_HOOK => {
                let value: WireLineCall = decode(frame.message_id, p)?;
                Ok(Self::OffHook {
                    line_instance: value.line_instance,
                    call_reference: value.call_reference,
                })
            }
            wire_id::ON_HOOK => decode_on_hook(p, frame.message_id),
            wire_id::OFF_HOOK_WITH_CALLING_PARTY => match protocol_version {
                19.. => decode_off_hook_with_calling_party::<25, 2>(p, frame.message_id),
                _ => decode_off_hook_with_calling_party::<24, 0>(p, frame.message_id),
            },
            wire_id::LINE_STAT_REQ => {
                let value: WireOneWord = decode(frame.message_id, p)?;
                Ok(Self::LineStatRequest {
                    line_instance: value.value,
                })
            }
            wire_id::CONFIG_STAT_REQ => Ok(Self::ConfigStatRequest),
            wire_id::TIME_DATE_REQ => Ok(Self::TimeDateRequest),
            wire_id::BUTTON_TEMPLATE_REQ => Ok(Self::ButtonTemplateRequest),
            wire_id::VERSION_REQ => Ok(Self::VersionRequest),
            wire_id::CAPABILITIES_RES => decode_capabilities_response(p, frame.message_id),
            wire_id::MEDIA_PORT_LIST => {
                let value: WireMediaPortList = decode(frame.message_id, p)?;
                let count = usize_from_wire(frame.message_id, "RTP ports", value.count)?;
                validate_media_port_count(frame.message_id, count)?;
                let rtp_ports = value.ports[..count]
                    .iter()
                    .copied()
                    .map(|port| {
                        u16::try_from(port).map_err(|_| CodecError::InvalidValue {
                            message_id: frame.message_id,
                            field: "RTP port",
                            value: u64::from(port),
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(Self::MediaPortList(MediaPortList { rtp_ports }))
            }
            wire_id::UPDATE_CAPABILITIES => {
                let expanded_layout = CapabilityUpdateVariant::Version1ExpandedVideo;
                let variant = match (
                    protocol_version,
                    p.len() >= expanded_layout.minimum_payload_bytes(protocol_version),
                ) {
                    (16.., true) => expanded_layout,
                    _ => CapabilityUpdateVariant::Version1,
                };
                CapabilityUpdate::decode(variant, protocol_version, p).map(Self::CapabilitiesUpdate)
            }
            wire_id::UPDATE_CAPABILITIES_V2 => {
                CapabilityUpdate::decode(CapabilityUpdateVariant::Version2, protocol_version, p)
                    .map(Self::CapabilitiesUpdate)
            }
            wire_id::UPDATE_CAPABILITIES_V3 => {
                CapabilityUpdate::decode(CapabilityUpdateVariant::Version3, protocol_version, p)
                    .map(Self::CapabilitiesUpdate)
            }
            wire_id::OPEN_MULTIMEDIA_RECEIVE_CHANNEL_ACK => {
                decode_open_multimedia_ack(p, protocol_version, frame.message_id)
                    .map(Self::OpenMultimediaReceiveChannelAck)
            }
            wire_id::SERVER_REQ => Ok(Self::ServerRequest),
            wire_id::ALARM => match p.len() {
                84 => {
                    let value: WireAlarmBase = decode(frame.message_id, p)?;
                    Ok(Self::Alarm {
                        severity: AlarmSeverity::from(value.severity),
                        text: value.text.text()?,
                        parameters: None,
                    })
                }
                92 => {
                    let value: WireAlarm = decode(frame.message_id, p)?;
                    Ok(Self::Alarm {
                        severity: AlarmSeverity::from(value.base.severity),
                        text: value.base.text.text()?,
                        parameters: Some([value.parameter_1, value.parameter_2]),
                    })
                }
                _ => Err(CodecError::InvalidLength(frame.message_id)),
            },
            wire_id::MULTICAST_MEDIA_RECEPTION_ACK => {
                validate_exact_payload(p, frame.message_id, 12)?;
                let value: WireMulticastReceptionAck = decode(frame.message_id, p)?;
                Ok(Self::MulticastMediaReceptionAck {
                    status: MediaStatus::from(value.status),
                    passthrough_party_id: value.passthrough_party_id.into(),
                    call_reference: value.call_reference.into(),
                })
            }
            wire_id::OPEN_RECEIVE_CHANNEL_ACK => match protocol_version {
                17.. => {
                    let value: WireOpenReceiveAckV17 = decode(frame.message_id, p)?;
                    Ok(Self::OpenReceiveChannelAck {
                        status: MediaStatus::from(value.status),
                        address: value.address.to_ip(frame.message_id)?,
                        port: decode_port(value.port, frame.message_id, "RTP port")?,
                        passthrough_party_id: value.passthrough_party_id,
                        call_reference: value.call_reference,
                    })
                }
                _ => {
                    let value: WireOpenReceiveAckV3 = decode(frame.message_id, p)?;
                    Ok(Self::OpenReceiveChannelAck {
                        status: MediaStatus::from(value.status),
                        address: value.address.to_ip(frame.message_id)?,
                        port: decode_port(value.port, frame.message_id, "RTP port")?,
                        passthrough_party_id: value.passthrough_party_id,
                        call_reference: value.call_reference,
                    })
                }
            },
            wire_id::SOFT_KEY_SET_REQ => Ok(Self::SoftKeySetRequest),
            wire_id::SOFT_KEY_TEMPLATE_REQ => Ok(Self::SoftKeyTemplateRequest),
            wire_id::SOFT_KEY_EVENT => {
                let value: WireSoftKeyEvent = decode(frame.message_id, p)?;
                Ok(Self::SoftKeyEvent {
                    event: value.event,
                    line_instance: value.line_instance,
                    call_reference: value.call_reference,
                })
            }
            wire_id::UNREGISTER => {
                let reason = if p.is_empty() {
                    0
                } else {
                    decode::<WireOneWord>(frame.message_id, p)?.value
                };
                Ok(Self::Unregister { reason })
            }
            wire_id::REGISTER_TOKEN_REQ => {
                let value: WireRegisterToken = decode(frame.message_id, p)?;
                let address = if value.ipv6_address.iter().any(|byte| *byte != 0) {
                    IpAddr::V6(Ipv6Addr::from(value.ipv6_address))
                } else {
                    IpAddr::V4(Ipv4Addr::from(value.ipv4_address))
                };
                Ok(Self::RegisterToken(RegisterTokenMessage {
                    device_id: DeviceId::new(value.device_id.text()?)?,
                    device_instance: value.device_instance,
                    address,
                    device_type: DeviceType::from(value.device_type),
                    flags: value.flags,
                }))
            }
            wire_id::SPCP_REGISTER_TOKEN_REQ => {
                let value: WireSpcpRegisterToken = decode(frame.message_id, p)?;
                Ok(Self::SpcpRegisterToken(SpcpRegisterTokenMessage {
                    device_id: DeviceId::new(value.device_id.text()?)?,
                    device_instance: value.device_instance,
                    address: Ipv4Addr::from(value.ipv4_address),
                    device_type: DeviceType::from(value.device_type),
                    max_streams: value.max_streams,
                }))
            }
            wire_id::HOOK_FLASH => {
                let value: WireLineCall = decode(frame.message_id, p)?;
                Ok(Self::HookFlash {
                    line_instance: value.line_instance,
                    call_reference: value.call_reference,
                })
            }
            wire_id::FORWARD_STAT_REQ => {
                let value: WireOneWord = decode(frame.message_id, p)?;
                Ok(Self::ForwardStatusRequest {
                    line_instance: value.value,
                })
            }
            wire_id::SPEED_DIAL_STAT_REQ => {
                let value: WireOneWord = decode(frame.message_id, p)?;
                Ok(Self::SpeedDialStatusRequest {
                    speed_dial_instance: value.value,
                })
            }
            wire_id::HEADSET_STATUS => {
                let value: WireOneWord = decode(frame.message_id, p)?;
                Ok(Self::HeadsetStatus {
                    enabled: value.value == 1,
                })
            }
            wire_id::MEDIA_RESOURCE_NOTIFICATION => {
                let value: WireMediaResourceNotification = decode(frame.message_id, p)?;
                Ok(Self::MediaResourceNotification(MediaResourceNotification {
                    device_type: DeviceType::from(value.device_type),
                    in_service_streams: value.in_service_streams,
                    max_streams_per_conference: value.max_streams_per_conference,
                    out_of_service_streams: value.out_of_service_streams,
                }))
            }
            wire_id::ACCESSORY_STATUS => {
                let value: WireAccessoryStatus = decode(frame.message_id, p)?;
                Ok(Self::MediaPathEvent {
                    path: MediaPathId::from(value.accessory),
                    event: MediaPathEvent::from(value.state),
                })
            }
            wire_id::MEDIA_PATH_CAPABILITY => {
                let value: WireAccessoryStatus = decode(frame.message_id, p)?;
                Ok(Self::MediaPathCapability {
                    path: MediaPathId::from(value.accessory),
                    capability: MediaPathCapability::from(value.state),
                })
            }
            wire_id::REGISTER_AVAILABLE_LINES => {
                let lines = if p.len() >= std::mem::size_of::<u32>() {
                    decode::<WireOneWord>(frame.message_id, p)?.value
                } else {
                    0
                };
                Ok(Self::RegisterAvailableLines { lines })
            }
            wire_id::DEVICE_TO_USER_DATA => {
                decode_user_data(p, frame.message_id).map(Self::DeviceToUserData)
            }
            wire_id::DEVICE_TO_USER_DATA_RESPONSE => {
                decode_user_data(p, frame.message_id).map(Self::DeviceToUserDataResponse)
            }
            wire_id::DEVICE_TO_USER_DATA_V1 => {
                decode_user_data_v1(p, frame.message_id).map(Self::DeviceToUserDataV1)
            }
            wire_id::DEVICE_TO_USER_DATA_RESPONSE_V1 => {
                decode_user_data_v1(p, frame.message_id).map(Self::DeviceToUserDataResponseV1)
            }
            wire_id::PORT_RESPONSE => {
                decode_port_response(p, protocol_version, frame.message_id).map(Self::PortResponse)
            }
            wire_id::SUBSCRIPTION_STAT_REQ => {
                let value: WireSubscriptionRequest = decode(frame.message_id, p)?;
                Ok(Self::SubscriptionStatusRequest(SubscriptionRequest {
                    transaction_id: value.transaction_id,
                    feature_id: value.feature_id,
                    timer_seconds: value.timer_seconds,
                    subscription_id: value.subscription_id.text()?,
                }))
            }
            wire_id::SUBSCRIBE_DTMF_PAYLOAD_RES => {
                let value: WireDtmfPayloadIdentity = decode(frame.message_id, p)?;
                Ok(Self::SubscribeDtmfPayloadResponse(
                    dtmf_payload_identity_from_wire(value),
                ))
            }
            wire_id::UNSUBSCRIBE_DTMF_PAYLOAD_RES => {
                let value: WireDtmfPayloadIdentity = decode(frame.message_id, p)?;
                Ok(Self::UnsubscribeDtmfPayloadResponse(
                    dtmf_payload_identity_from_wire(value),
                ))
            }
            wire_id::SERVICE_URL_STAT_REQ => {
                let value: WireOneWord = decode(frame.message_id, p)?;
                Ok(Self::ServiceUrlStatusRequest { index: value.value })
            }
            wire_id::FEATURE_STAT_REQ => {
                let value: WireFeatureStatusRequest = decode(frame.message_id, p)?;
                Ok(Self::FeatureStatusRequest {
                    index: value.index,
                    capabilities: value.capabilities,
                })
            }
            wire_id::MEDIA_TRANSMISSION_FAILURE => match protocol_version {
                17.. => {
                    let value: WireMediaFailureV17 = decode(frame.message_id, p)?;
                    Ok(Self::MediaTransmissionFailure {
                        conference_id: value.conference_id,
                        passthrough_party_id: value.passthrough_party_id,
                        address: value.address.to_ip(frame.message_id)?,
                        port: decode_port(value.port, frame.message_id, "RTP port")?,
                        call_reference: value.call_reference,
                        status: MediaStatus::UnspecifiedError,
                    })
                }
                _ => {
                    let value: WireMediaFailureV3 = decode(frame.message_id, p)?;
                    Ok(Self::MediaTransmissionFailure {
                        conference_id: value.conference_id,
                        passthrough_party_id: value.passthrough_party_id,
                        address: value.address.to_ip(frame.message_id)?,
                        port: decode_port(value.port, frame.message_id, "RTP port")?,
                        call_reference: value.call_reference,
                        status: MediaStatus::UnspecifiedError,
                    })
                }
            },
            wire_id::CONNECTION_STATISTICS_RES => {
                decode_connection_statistics(p, protocol_version, frame.message_id)
                    .map(Self::ConnectionStatisticsResponse)
            }
            wire_id::START_MEDIA_TRANSMISSION_ACK => {
                decode_start_media_ack(p, protocol_version, frame.message_id)
                    .map(Self::StartMediaTransmissionAck)
            }
            wire_id::START_MULTIMEDIA_TRANSMISSION_ACK => {
                decode_start_multimedia_ack(p, protocol_version, frame.message_id)
                    .map(Self::StartMultimediaTransmissionAck)
            }
            wire_id::EXTENSION_DEVICE_CAPABILITIES => {
                let value: WireExtensionDeviceCapabilities = decode(frame.message_id, p)?;
                Ok(Self::ExtensionDeviceCapabilities(
                    ExtensionDeviceCapabilities {
                        unknown_1: value.unknown_1,
                        unknown_2: value.unknown_2,
                        unknown_3: value.unknown_3,
                        description: value.description.text()?,
                    },
                ))
            }
            wire_id::LOCATION_INFO => {
                let value: WireLocationInfo = decode(frame.message_id, p)?;
                validate_zero_payload(&value.alignment, frame.message_id, 3)?;
                Ok(Self::LocationInfo {
                    xml: value.xml.text()?,
                })
            }
            wire_id::XML_ALARM => {
                XmlAlarmMessage::from_wire_payload(p.to_vec()).map(Self::XmlAlarm)
            }
            wire_id::CALL_COUNT_REQ => {
                let payload = match p.len() {
                    0 => CallCountRequestPayload::Empty,
                    4 => CallCountRequestPayload::LegacyWord(
                        decode::<WireOneWord>(frame.message_id, p)?.value,
                    ),
                    CALL_COUNT_REQUEST_EXTENDED_BYTES => {
                        let extended =
                            p.as_slice()
                                .try_into()
                                .map_err(|_| CodecError::InvalidValue {
                                    message_id: frame.message_id,
                                    field: "call-count request payload length",
                                    value: p.len() as u64,
                                })?;
                        CallCountRequestPayload::Extended(extended)
                    }
                    _ => {
                        return Err(CodecError::InvalidValue {
                            message_id: frame.message_id,
                            field: "call-count request payload length",
                            value: p.len() as u64,
                        });
                    }
                };
                Ok(Self::CallCountRequest(payload))
            }
            wire_id::CREATE_CONFERENCE_RES => {
                validate_conference_data_length(p, frame.message_id, 12, 8)?;
                let value: WireConferenceResponse = decode_zero_padded(frame.message_id, p)?;
                Ok(Self::CreateConferenceResponse(CreateConferenceResponse {
                    conference_id: value.conference_id.into(),
                    result: CreateConferenceResult::from(value.result),
                    passthrough_data: value.passthrough_data,
                }))
            }
            wire_id::DELETE_CONFERENCE_RES => {
                validate_exact_payload(p, frame.message_id, 8)?;
                let value: WireCallParty = decode(frame.message_id, p)?;
                Ok(Self::DeleteConferenceResponse {
                    conference_id: value.call_reference.into(),
                    result: DeleteConferenceResult::from(value.passthrough_party_id),
                })
            }
            wire_id::MODIFY_CONFERENCE_RES => {
                validate_conference_data_length(p, frame.message_id, 12, 8)?;
                let value: WireConferenceResponse = decode_zero_padded(frame.message_id, p)?;
                Ok(Self::ModifyConferenceResponse(ModifyConferenceResponse {
                    conference_id: value.conference_id.into(),
                    result: ModifyConferenceResult::from(value.result),
                    passthrough_data: value.passthrough_data,
                }))
            }
            wire_id::AUDIT_CONFERENCE_RES => {
                if p.len() < 8 {
                    return Err(CodecError::Truncated {
                        message_id: frame.message_id,
                        needed: 8,
                        actual: p.len(),
                    });
                }
                let number_of_entries = usize_from_wire(
                    frame.message_id,
                    "conference audit entries",
                    u32::from_le_bytes(p[4..8].try_into().expect("validated audit header")),
                )?;
                if number_of_entries > MAX_AUDIT_CONFERENCE_ENTRIES {
                    return Err(CodecError::CountTooLarge {
                        message_id: frame.message_id,
                        field: "conference audit entries",
                        count: number_of_entries,
                        maximum: MAX_AUDIT_CONFERENCE_ENTRIES,
                    });
                }
                validate_exact_payload(p, frame.message_id, 8 + number_of_entries * 76)?;
                let value: WireAuditConferenceResponse = decode(frame.message_id, p)?;
                Ok(Self::AuditConferenceResponse(AuditConferenceResponse {
                    last: value.last,
                    entries: value
                        .entries
                        .into_iter()
                        .map(|entry| {
                            Ok(AuditConferenceEntry {
                                conference_id: entry.conference_id.into(),
                                resource_type: ConferenceResourceType::from(entry.resource_type),
                                reserved_participants: entry.reserved_participants,
                                active_participants: entry.active_participants,
                                application_id: entry.application_id.into(),
                                application_conference_id: entry
                                    .application_conference_id
                                    .text()?,
                                application_data: entry.application_data.text()?,
                            })
                        })
                        .collect::<Result<Vec<_>, CodecError>>()?,
                }))
            }
            wire_id::ADD_PARTICIPANT_RES => {
                validate_payload_bounds(p, frame.message_id, 12, 272)?;
                let value: WireAddParticipantResponseHeader = decode_prefix(frame.message_id, p)?;
                let identifier_end = if p.len() == 272 {
                    if p[269..].iter().any(|byte| *byte != 0) {
                        return Err(CodecError::InvalidValue {
                            message_id: frame.message_id,
                            field: "AddParticipantResponse alignment",
                            value: 1,
                        });
                    }
                    269
                } else {
                    p.len()
                };
                let identifier = &p[12..identifier_end];
                let bridge_participant_id =
                    BoundedBytes::try_from(identifier).map_err(|error| {
                        CodecError::CountTooLarge {
                            message_id: frame.message_id,
                            field: "bridge participant identifier",
                            count: error.actual,
                            maximum: error.maximum,
                        }
                    })?;
                Ok(Self::AddParticipantResponse(AddParticipantResponse {
                    conference_id: value.conference_id.into(),
                    call_reference: value.call_reference.into(),
                    result: AddParticipantResult::from(value.result),
                    bridge_participant_id,
                }))
            }
            wire_id::AUDIT_PARTICIPANT_RES => {
                if p.len() < 16 {
                    return Err(CodecError::Truncated {
                        message_id: frame.message_id,
                        needed: 16,
                        actual: p.len(),
                    });
                }
                let participant_entries = &p[16..];
                if participant_entries.len() > MAX_AUDIT_PARTICIPANT_DATA {
                    return Err(CodecError::CountTooLarge {
                        message_id: frame.message_id,
                        field: "participant audit data",
                        count: participant_entries.len(),
                        maximum: MAX_AUDIT_PARTICIPANT_DATA,
                    });
                }
                let value: WireAuditParticipantResponseHeader = decode(frame.message_id, &p[..16])?;
                Ok(Self::AuditParticipantResponse(AuditParticipantResponse {
                    result: AuditParticipantResult::from(value.result),
                    last: value.last,
                    conference_id: value.conference_id.into(),
                    number_of_entries: value.number_of_entries,
                    participant_entries: participant_entries.to_vec(),
                }))
            }
            _ => {
                let id = frame.message_type();
                if id.is_known() {
                    preserve_known_message(frame, id).map(Self::KnownOpaque)
                } else {
                    Ok(Self::Unknown(RawMessage {
                        message_id: frame.message_id,
                        protocol_version: frame.protocol_version,
                        payload: frame.payload,
                    }))
                }
            }
        }
    }

    /// Canonically encode a phone-to-server message.
    pub fn encode(&self, protocol: ProtocolVersion) -> Result<Vec<u8>, CodecError> {
        let (message_id, payload, header_protocol) = self.payload(protocol)?;
        reject_non_station_route(
            message_id,
            MessageRoute::StationToControl,
            "station-to-control",
        )?;
        Frame::new(header_protocol, message_id, payload).encode()
    }

    fn encode_unchecked(&self, protocol: ProtocolVersion) -> Result<Vec<u8>, CodecError> {
        let (message_id, payload, header_protocol) = self.payload(protocol)?;
        Frame::new(header_protocol, message_id, payload).encode()
    }

    fn payload(&self, protocol: ProtocolVersion) -> Result<(u32, Vec<u8>, u32), CodecError> {
        let mut payload = Vec::new();
        let mut header_protocol = protocol.wire();
        let message_id = match self {
            Self::KeepAlive => {
                header_protocol = 0;
                wire_id::KEEP_ALIVE
            }
            Self::Register(registration) => {
                header_protocol = 0;
                let feature_bytes = registration.features.bits().to_le_bytes();
                let wire = registration.wire.unwrap_or(RegistrationWireDetails {
                    layout: RegistrationWireLayout::default(),
                    station_user_id: 0,
                    station_instance: 1,
                    max_streams: 0,
                    active_streams: 0,
                    mac_address_and_padding: [0; 12],
                    max_conferences: 0,
                    active_conferences: 0,
                    ipv4_address_scope: 0,
                    max_lines: 0,
                    ipv6_address_scope: 0,
                });
                let prefix_bytes = registration_prefix_bytes(wire.layout)?;
                validate_registration_fields(registration, wire, prefix_bytes)?;
                let alternate = wire.layout == RegistrationWireLayout::Alternate32;
                let device_id = WireFixedText::new(
                    wire_id::REGISTER,
                    "device ID",
                    registration.device_id.as_str(),
                )?;
                let advertised_protocol = registration.advertised_protocol.unwrap_or(0);
                let protocol_features = if alternate {
                    [0; 4]
                } else {
                    [
                        advertised_protocol.min(u32::from(u8::MAX)) as u8,
                        feature_bytes[1],
                        feature_bytes[2],
                        feature_bytes[3],
                    ]
                };
                let reported_address = if alternate {
                    [advertised_protocol.min(u32::from(u8::MAX)) as u8, 0, 0, 0]
                } else {
                    registration
                        .reported_address
                        .unwrap_or(Ipv4Addr::UNSPECIFIED)
                        .octets()
                };
                payload = encode(
                    wire_id::REGISTER,
                    &WireRegister {
                        device_id,
                        station_user_id: wire.station_user_id,
                        station_instance: wire.station_instance,
                        reported_address,
                        device_type: registration.device_type.wire_value(),
                        max_streams: wire.max_streams,
                        active_streams: wire.active_streams,
                        protocol_features,
                        max_conferences: wire.max_conferences,
                        active_conferences: wire.active_conferences,
                        mac_address: wire.mac_address_and_padding,
                        ipv4_address_scope: wire.ipv4_address_scope,
                        max_lines: wire.max_lines,
                        ipv6_address: registration
                            .reported_ipv6_address
                            .unwrap_or(Ipv6Addr::UNSPECIFIED)
                            .octets(),
                        ipv6_address_scope: wire.ipv6_address_scope,
                        firmware: WireFixedText::new(
                            wire_id::REGISTER,
                            "firmware",
                            &registration.firmware,
                        )?,
                    },
                )?;
                payload.truncate(prefix_bytes);
                if !alternate {
                    payload.extend_from_slice(registration.configuration_version_stamp.as_bytes());
                }
                wire_id::REGISTER
            }
            Self::IpPort { rtp_port } => {
                payload = encode(
                    wire_id::IP_PORT,
                    &WireOneWord {
                        value: u32::from(*rtp_port),
                    },
                )?;
                wire_id::IP_PORT
            }
            Self::KeypadButton {
                button,
                line_instance,
                call_reference,
                wire_layout,
            } => {
                payload = match wire_layout {
                    Some(KeypadButtonWireLayout::LegacyButtonOnly) => encode(
                        wire_id::KEYPAD_BUTTON,
                        &WireKeypadButtonLegacy {
                            button: button.keypad_value(),
                        },
                    )?,
                    Some(KeypadButtonWireLayout::WithCallIdentity) => encode(
                        wire_id::KEYPAD_BUTTON,
                        &WireKeypadButtonWithCall {
                            base: WireKeypadButtonLegacy {
                                button: button.keypad_value(),
                            },
                            line_instance: *line_instance,
                            call_reference: *call_reference,
                        },
                    )?,
                    None => encode(
                        wire_id::KEYPAD_BUTTON,
                        &WireKeypadButton {
                            base: WireKeypadButtonWithCall {
                                base: WireKeypadButtonLegacy {
                                    button: button.keypad_value(),
                                },
                                line_instance: *line_instance,
                                call_reference: *call_reference,
                            },
                            keypad_union: 0,
                            reserved: 0,
                        },
                    )?,
                };
                wire_id::KEYPAD_BUTTON
            }
            Self::EnblocCall {
                called_party,
                line_instance,
            } => {
                payload = encode_enbloc(called_party, *line_instance, protocol.wire())?;
                wire_id::ENBLOC_CALL
            }
            Self::Stimulus {
                stimulus,
                instance,
                call_reference,
                status,
            } => {
                payload = encode(
                    wire_id::STIMULUS,
                    &WireStimulus {
                        stimulus: stimulus.wire_value(),
                        instance: *instance,
                        call_reference: *call_reference,
                        status: *status,
                    },
                )?;
                wire_id::STIMULUS
            }
            Self::OffHook {
                line_instance,
                call_reference,
            } => {
                payload = encode(
                    wire_id::OFF_HOOK,
                    &WireLineCall {
                        line_instance: *line_instance,
                        call_reference: *call_reference,
                    },
                )?;
                wire_id::OFF_HOOK
            }
            Self::OnHook {
                line_instance,
                call_reference,
            } => {
                payload = encode(
                    wire_id::ON_HOOK,
                    &WireLineCall {
                        line_instance: *line_instance,
                        call_reference: *call_reference,
                    },
                )?;
                wire_id::ON_HOOK
            }
            Self::OffHookWithCallingParty {
                calling_party_number,
                voice_mailbox,
                line_instance,
            } => {
                payload = match protocol.wire() {
                    19.. => encode_off_hook_with_calling_party::<25, 2>(
                        calling_party_number,
                        voice_mailbox,
                        *line_instance,
                    ),
                    _ => encode_off_hook_with_calling_party::<24, 0>(
                        calling_party_number,
                        voice_mailbox,
                        *line_instance,
                    ),
                }?;
                wire_id::OFF_HOOK_WITH_CALLING_PARTY
            }
            Self::HookFlash {
                line_instance,
                call_reference,
            } => {
                payload = encode(
                    wire_id::HOOK_FLASH,
                    &WireLineCall {
                        line_instance: *line_instance,
                        call_reference: *call_reference,
                    },
                )?;
                wire_id::HOOK_FLASH
            }
            Self::ForwardStatusRequest { line_instance } => {
                payload = encode(
                    wire_id::FORWARD_STAT_REQ,
                    &WireOneWord {
                        value: *line_instance,
                    },
                )?;
                wire_id::FORWARD_STAT_REQ
            }
            Self::SpeedDialStatusRequest {
                speed_dial_instance,
            } => {
                payload = encode(
                    wire_id::SPEED_DIAL_STAT_REQ,
                    &WireOneWord {
                        value: *speed_dial_instance,
                    },
                )?;
                wire_id::SPEED_DIAL_STAT_REQ
            }
            Self::LineStatRequest { line_instance } => {
                payload = encode(
                    wire_id::LINE_STAT_REQ,
                    &WireOneWord {
                        value: *line_instance,
                    },
                )?;
                wire_id::LINE_STAT_REQ
            }
            Self::ConfigStatRequest => wire_id::CONFIG_STAT_REQ,
            Self::TimeDateRequest => wire_id::TIME_DATE_REQ,
            Self::ButtonTemplateRequest => wire_id::BUTTON_TEMPLATE_REQ,
            Self::VersionRequest => wire_id::VERSION_REQ,
            Self::CapabilitiesResponse(capabilities) => {
                payload = encode_capabilities_response(capabilities)?;
                wire_id::CAPABILITIES_RES
            }
            Self::MediaPortList(message) => {
                validate_media_port_count(wire_id::MEDIA_PORT_LIST, message.rtp_ports.len())?;
                let mut ports = [0; MEDIA_PORT_LIST_MAX_PORTS];
                for (target, port) in ports.iter_mut().zip(&message.rtp_ports) {
                    *target = u32::from(*port);
                }
                payload = encode(
                    wire_id::MEDIA_PORT_LIST,
                    &WireMediaPortList {
                        count: wire_count(
                            wire_id::MEDIA_PORT_LIST,
                            "RTP ports",
                            message.rtp_ports.len(),
                        )?,
                        ports,
                    },
                )?;
                wire_id::MEDIA_PORT_LIST
            }
            Self::CapabilitiesUpdate(update) => {
                payload.extend_from_slice(update.raw_payload());
                update.variant().message_id()
            }
            Self::OpenMultimediaReceiveChannelAck(ack) => {
                payload = encode_open_multimedia_ack(*ack, protocol)?;
                wire_id::OPEN_MULTIMEDIA_RECEIVE_CHANNEL_ACK
            }
            Self::ServerRequest => wire_id::SERVER_REQ,
            Self::Alarm {
                severity,
                text,
                parameters,
            } => {
                let text = WireFixedText::new(wire_id::ALARM, "alarm text", text)?;
                let base = WireAlarmBase {
                    severity: severity.wire_value(),
                    text,
                };
                payload = match parameters {
                    Some([parameter_1, parameter_2]) => encode(
                        wire_id::ALARM,
                        &WireAlarm {
                            base,
                            parameter_1: *parameter_1,
                            parameter_2: *parameter_2,
                        },
                    ),
                    None => encode(wire_id::ALARM, &base),
                }?;
                wire_id::ALARM
            }
            Self::MulticastMediaReceptionAck {
                status,
                passthrough_party_id,
                call_reference,
            } => {
                payload = encode(
                    wire_id::MULTICAST_MEDIA_RECEPTION_ACK,
                    &WireMulticastReceptionAck {
                        status: status.wire_value(),
                        passthrough_party_id: passthrough_party_id.get(),
                        call_reference: call_reference.get(),
                    },
                )?;
                wire_id::MULTICAST_MEDIA_RECEPTION_ACK
            }
            Self::OpenReceiveChannelAck {
                status,
                address,
                port,
                passthrough_party_id,
                call_reference,
            } => {
                payload = match protocol.wire() {
                    17.. => encode(
                        wire_id::OPEN_RECEIVE_CHANNEL_ACK,
                        &WireOpenReceiveAckV17 {
                            status: status.wire_value(),
                            address: WireExtendedAddress::from_ip(*address),
                            port: u32::from(*port),
                            passthrough_party_id: *passthrough_party_id,
                            call_reference: *call_reference,
                        },
                    ),
                    _ => encode(
                        wire_id::OPEN_RECEIVE_CHANNEL_ACK,
                        &WireOpenReceiveAckV3 {
                            status: status.wire_value(),
                            address: WireIpv4Address::from_ip(
                                *address,
                                wire_id::OPEN_RECEIVE_CHANNEL_ACK,
                                "IP address family for this protocol version",
                            )?,
                            port: u32::from(*port),
                            passthrough_party_id: *passthrough_party_id,
                            call_reference: *call_reference,
                        },
                    ),
                }?;
                wire_id::OPEN_RECEIVE_CHANNEL_ACK
            }
            Self::SoftKeySetRequest => wire_id::SOFT_KEY_SET_REQ,
            Self::SoftKeyTemplateRequest => wire_id::SOFT_KEY_TEMPLATE_REQ,
            Self::SoftKeyEvent {
                event,
                line_instance,
                call_reference,
            } => {
                payload = encode(
                    wire_id::SOFT_KEY_EVENT,
                    &WireSoftKeyEvent {
                        event: *event,
                        line_instance: *line_instance,
                        call_reference: *call_reference,
                    },
                )?;
                wire_id::SOFT_KEY_EVENT
            }
            Self::Unregister { reason } => {
                payload = encode(wire_id::UNREGISTER, &WireOneWord { value: *reason })?;
                wire_id::UNREGISTER
            }
            Self::RegisterToken(token) => {
                let (ipv4_address, ipv6_address) = match token.address {
                    IpAddr::V4(address) => (address.octets(), [0; 16]),
                    IpAddr::V6(address) => ([0; 4], address.octets()),
                };
                payload = encode(
                    wire_id::REGISTER_TOKEN_REQ,
                    &WireRegisterToken {
                        device_id: WireFixedText::new(
                            wire_id::REGISTER_TOKEN_REQ,
                            "device ID",
                            token.device_id.as_str(),
                        )?,
                        device_instance: token.device_instance,
                        ipv4_address,
                        device_type: token.device_type.wire_value(),
                        ipv6_address,
                        flags: token.flags,
                    },
                )?;
                wire_id::REGISTER_TOKEN_REQ
            }
            Self::SpcpRegisterToken(token) => {
                payload = encode(
                    wire_id::SPCP_REGISTER_TOKEN_REQ,
                    &WireSpcpRegisterToken {
                        device_id: WireFixedText::new(
                            wire_id::SPCP_REGISTER_TOKEN_REQ,
                            "device ID",
                            token.device_id.as_str(),
                        )?,
                        reserved: 0,
                        device_instance: token.device_instance,
                        ipv4_address: u32::from(token.address),
                        device_type: token.device_type.wire_value(),
                        max_streams: token.max_streams,
                    },
                )?;
                wire_id::SPCP_REGISTER_TOKEN_REQ
            }
            Self::ConnectionStatisticsResponse(statistics) => {
                payload = encode_connection_statistics(statistics, protocol)?;
                wire_id::CONNECTION_STATISTICS_RES
            }
            Self::HeadsetStatus { enabled } => {
                payload = encode(
                    wire_id::HEADSET_STATUS,
                    &WireOneWord {
                        value: u32::from(*enabled),
                    },
                )?;
                wire_id::HEADSET_STATUS
            }
            Self::MediaResourceNotification(notification) => {
                payload = encode(
                    wire_id::MEDIA_RESOURCE_NOTIFICATION,
                    &WireMediaResourceNotification {
                        device_type: notification.device_type.wire_value(),
                        in_service_streams: notification.in_service_streams,
                        max_streams_per_conference: notification.max_streams_per_conference,
                        out_of_service_streams: notification.out_of_service_streams,
                    },
                )?;
                wire_id::MEDIA_RESOURCE_NOTIFICATION
            }
            Self::MediaPathEvent { path, event } => {
                payload = encode(
                    wire_id::ACCESSORY_STATUS,
                    &WireAccessoryStatus {
                        accessory: path.wire_value(),
                        state: event.wire_value(),
                    },
                )?;
                wire_id::ACCESSORY_STATUS
            }
            Self::MediaPathCapability { path, capability } => {
                payload = encode(
                    wire_id::MEDIA_PATH_CAPABILITY,
                    &WireAccessoryStatus {
                        accessory: path.wire_value(),
                        state: capability.wire_value(),
                    },
                )?;
                wire_id::MEDIA_PATH_CAPABILITY
            }
            Self::MediaTransmissionFailure {
                conference_id,
                passthrough_party_id,
                address,
                port,
                call_reference,
                ..
            } => {
                payload = match protocol.wire() {
                    17.. => encode(
                        wire_id::MEDIA_TRANSMISSION_FAILURE,
                        &WireMediaFailureV17 {
                            conference_id: *conference_id,
                            passthrough_party_id: *passthrough_party_id,
                            address: WireExtendedAddress::from_ip(*address),
                            port: u32::from(*port),
                            call_reference: *call_reference,
                        },
                    ),
                    _ => encode(
                        wire_id::MEDIA_TRANSMISSION_FAILURE,
                        &WireMediaFailureV3 {
                            conference_id: *conference_id,
                            passthrough_party_id: *passthrough_party_id,
                            address: WireIpv4Address::from_ip(
                                *address,
                                wire_id::MEDIA_TRANSMISSION_FAILURE,
                                "IP address family for this protocol version",
                            )?,
                            port: u32::from(*port),
                            call_reference: *call_reference,
                        },
                    ),
                }?;
                wire_id::MEDIA_TRANSMISSION_FAILURE
            }
            Self::RegisterAvailableLines { lines } => {
                payload = encode(
                    wire_id::REGISTER_AVAILABLE_LINES,
                    &WireOneWord { value: *lines },
                )?;
                wire_id::REGISTER_AVAILABLE_LINES
            }
            Self::ServiceUrlStatusRequest { index } => {
                payload = encode(
                    wire_id::SERVICE_URL_STAT_REQ,
                    &WireOneWord { value: *index },
                )?;
                wire_id::SERVICE_URL_STAT_REQ
            }
            Self::FeatureStatusRequest {
                index,
                capabilities,
            } => {
                payload = encode(
                    wire_id::FEATURE_STAT_REQ,
                    &WireFeatureStatusRequest {
                        index: *index,
                        capabilities: *capabilities,
                    },
                )?;
                wire_id::FEATURE_STAT_REQ
            }
            Self::StartMediaTransmissionAck(ack) => {
                payload = encode_start_media_ack(ack, protocol)?;
                wire_id::START_MEDIA_TRANSMISSION_ACK
            }
            Self::StartMultimediaTransmissionAck(ack) => {
                payload = encode_start_multimedia_ack(*ack, protocol)?;
                wire_id::START_MULTIMEDIA_TRANSMISSION_ACK
            }
            Self::ExtensionDeviceCapabilities(capabilities) => {
                payload = encode(
                    wire_id::EXTENSION_DEVICE_CAPABILITIES,
                    &WireExtensionDeviceCapabilities {
                        unknown_1: capabilities.unknown_1,
                        unknown_2: capabilities.unknown_2,
                        unknown_3: capabilities.unknown_3,
                        description: WireFixedText::new(
                            wire_id::EXTENSION_DEVICE_CAPABILITIES,
                            "extension-device capability description",
                            &capabilities.description,
                        )?,
                    },
                )?;
                wire_id::EXTENSION_DEVICE_CAPABILITIES
            }
            Self::DeviceToUserData(data) => {
                payload = encode_user_data(data, wire_id::DEVICE_TO_USER_DATA)?;
                wire_id::DEVICE_TO_USER_DATA
            }
            Self::DeviceToUserDataResponse(data) => {
                payload = encode_user_data(data, wire_id::DEVICE_TO_USER_DATA_RESPONSE)?;
                wire_id::DEVICE_TO_USER_DATA_RESPONSE
            }
            Self::DeviceToUserDataV1(data) => {
                payload = encode_user_data_v1(data, wire_id::DEVICE_TO_USER_DATA_V1)?;
                wire_id::DEVICE_TO_USER_DATA_V1
            }
            Self::DeviceToUserDataResponseV1(data) => {
                payload = encode_user_data_v1(data, wire_id::DEVICE_TO_USER_DATA_RESPONSE_V1)?;
                wire_id::DEVICE_TO_USER_DATA_RESPONSE_V1
            }
            Self::PortResponse(endpoint) => {
                payload = encode_port_response(endpoint, protocol)?;
                wire_id::PORT_RESPONSE
            }
            Self::SubscriptionStatusRequest(subscription) => {
                payload = encode(
                    wire_id::SUBSCRIPTION_STAT_REQ,
                    &WireSubscriptionRequest {
                        transaction_id: subscription.transaction_id,
                        feature_id: subscription.feature_id,
                        timer_seconds: subscription.timer_seconds,
                        subscription_id: WireFixedText::new(
                            wire_id::SUBSCRIPTION_STAT_REQ,
                            "subscription ID",
                            &subscription.subscription_id,
                        )?,
                    },
                )?;
                wire_id::SUBSCRIPTION_STAT_REQ
            }
            Self::SubscribeDtmfPayloadResponse(identity) => {
                payload = encode(
                    wire_id::SUBSCRIBE_DTMF_PAYLOAD_RES,
                    &dtmf_payload_identity_to_wire(*identity),
                )?;
                wire_id::SUBSCRIBE_DTMF_PAYLOAD_RES
            }
            Self::UnsubscribeDtmfPayloadResponse(identity) => {
                payload = encode(
                    wire_id::UNSUBSCRIBE_DTMF_PAYLOAD_RES,
                    &dtmf_payload_identity_to_wire(*identity),
                )?;
                wire_id::UNSUBSCRIBE_DTMF_PAYLOAD_RES
            }
            Self::LocationInfo { xml } => {
                payload = encode(
                    wire_id::LOCATION_INFO,
                    &WireLocationInfo {
                        xml: WireFixedText::new(wire_id::LOCATION_INFO, "location XML", xml)?,
                        alignment: [0; 3],
                    },
                )?;
                wire_id::LOCATION_INFO
            }
            Self::XmlAlarm(message) => {
                payload = message.wire_payload().to_vec();
                wire_id::XML_ALARM
            }
            Self::CallCountRequest(request) => {
                match request {
                    CallCountRequestPayload::Empty => {}
                    CallCountRequestPayload::LegacyWord(value) => {
                        payload = encode(wire_id::CALL_COUNT_REQ, &WireOneWord { value: *value })?;
                    }
                    CallCountRequestPayload::Extended(extended) => {
                        payload.extend_from_slice(extended);
                    }
                }
                wire_id::CALL_COUNT_REQ
            }
            Self::CreateConferenceResponse(response) => {
                payload = encode(
                    wire_id::CREATE_CONFERENCE_RES,
                    &WireConferenceResponse {
                        conference_id: response.conference_id.get(),
                        result: response.result.wire_value(),
                        data_length: validate_conference_data_for_encode(
                            wire_id::CREATE_CONFERENCE_RES,
                            &response.passthrough_data,
                        )?,
                        passthrough_data: response.passthrough_data.clone(),
                    },
                )?;
                wire_id::CREATE_CONFERENCE_RES
            }
            Self::DeleteConferenceResponse {
                conference_id,
                result,
            } => {
                payload = encode(
                    wire_id::DELETE_CONFERENCE_RES,
                    &WireCallParty {
                        call_reference: conference_id.get(),
                        passthrough_party_id: result.wire_value(),
                    },
                )?;
                wire_id::DELETE_CONFERENCE_RES
            }
            Self::ModifyConferenceResponse(response) => {
                payload = encode(
                    wire_id::MODIFY_CONFERENCE_RES,
                    &WireConferenceResponse {
                        conference_id: response.conference_id.get(),
                        result: response.result.wire_value(),
                        data_length: validate_conference_data_for_encode(
                            wire_id::MODIFY_CONFERENCE_RES,
                            &response.passthrough_data,
                        )?,
                        passthrough_data: response.passthrough_data.clone(),
                    },
                )?;
                wire_id::MODIFY_CONFERENCE_RES
            }
            Self::AuditConferenceResponse(response) => {
                if response.entries.len() > MAX_AUDIT_CONFERENCE_ENTRIES {
                    return Err(CodecError::CountTooLarge {
                        message_id: wire_id::AUDIT_CONFERENCE_RES,
                        field: "conference audit entries",
                        count: response.entries.len(),
                        maximum: MAX_AUDIT_CONFERENCE_ENTRIES,
                    });
                }
                payload = encode(
                    wire_id::AUDIT_CONFERENCE_RES,
                    &WireAuditConferenceResponse {
                        last: response.last,
                        number_of_entries: wire_count(
                            wire_id::AUDIT_CONFERENCE_RES,
                            "conference audit entries",
                            response.entries.len(),
                        )?,
                        entries: response
                            .entries
                            .iter()
                            .map(|entry| {
                                Ok(WireAuditConferenceEntry {
                                    conference_id: entry.conference_id.get(),
                                    resource_type: entry.resource_type.wire_value(),
                                    reserved_participants: entry.reserved_participants,
                                    active_participants: entry.active_participants,
                                    application_id: entry.application_id.get(),
                                    application_conference_id: WireFixedText::new(
                                        wire_id::AUDIT_CONFERENCE_RES,
                                        "application conference ID",
                                        &entry.application_conference_id,
                                    )?,
                                    application_data: WireFixedText::new(
                                        wire_id::AUDIT_CONFERENCE_RES,
                                        "application data",
                                        &entry.application_data,
                                    )?,
                                })
                            })
                            .collect::<Result<Vec<_>, CodecError>>()?,
                    },
                )?;
                wire_id::AUDIT_CONFERENCE_RES
            }
            Self::AddParticipantResponse(response) => {
                payload = encode(
                    wire_id::ADD_PARTICIPANT_RES,
                    &WireAddParticipantResponseHeader {
                        conference_id: response.conference_id.get(),
                        call_reference: response.call_reference.get(),
                        result: response.result.wire_value(),
                    },
                )?;
                payload.extend_from_slice(response.bridge_participant_id.as_bytes());
                payload.resize(269, 0);
                payload.extend_from_slice(&[0; 3]);
                wire_id::ADD_PARTICIPANT_RES
            }
            Self::AuditParticipantResponse(response) => {
                if response.participant_entries.len() > MAX_AUDIT_PARTICIPANT_DATA {
                    return Err(CodecError::CountTooLarge {
                        message_id: wire_id::AUDIT_PARTICIPANT_RES,
                        field: "participant audit data",
                        count: response.participant_entries.len(),
                        maximum: MAX_AUDIT_PARTICIPANT_DATA,
                    });
                }
                payload = encode(
                    wire_id::AUDIT_PARTICIPANT_RES,
                    &WireAuditParticipantResponseHeader {
                        result: response.result.wire_value(),
                        last: response.last,
                        conference_id: response.conference_id.get(),
                        number_of_entries: response.number_of_entries,
                    },
                )?;
                payload.extend_from_slice(&response.participant_entries);
                wire_id::AUDIT_PARTICIPANT_RES
            }
            Self::KnownOpaque(message) => {
                ensure_preserve_only(message.id)?;
                return Ok((
                    message.id.wire_value(),
                    message.payload.as_bytes().to_vec(),
                    message.protocol_version,
                ));
            }
            Self::Unknown(message) => {
                return Ok((
                    message.message_id,
                    message.payload.clone(),
                    message.protocol_version,
                ));
            }
        };
        pad_typed_payload(message_id, &mut payload);
        Ok((message_id, payload, header_protocol))
    }
}

fn encode_connection_statistics(
    statistics: &ConnectionStatistics,
    protocol: ProtocolVersion,
) -> Result<Vec<u8>, CodecError> {
    let tail = WireConnectionStatisticsTail {
        counters: WireConnectionStatisticsCounters {
            packets_sent: statistics.packets_sent,
            octets_sent: statistics.octets_sent,
            packets_received: statistics.packets_received,
            octets_received: statistics.octets_received,
            packets_lost: statistics.packets_lost,
            jitter_millis: statistics.jitter_millis,
            latency_millis: statistics.latency_millis,
        },
        quality_size: u32::try_from(statistics.quality.as_bytes().len()).map_err(|_| {
            CodecError::CountTooLarge {
                message_id: wire_id::CONNECTION_STATISTICS_RES,
                field: "quality statistics",
                count: statistics.quality.as_bytes().len(),
                maximum: CONNECTION_QUALITY_MAX_BYTES,
            }
        })?,
    };
    match protocol.wire() {
        19.. => encode(
            wire_id::CONNECTION_STATISTICS_RES,
            &WireConnectionStatisticsV19 {
                directory_number: WireAlignedText::new(
                    wire_id::CONNECTION_STATISTICS_RES,
                    "directory number",
                    &statistics.directory_number,
                )?,
                call_reference: statistics.call_reference,
                processing: statistics.processing.wire_value(),
                statistics: tail,
                quality: statistics.quality.as_bytes().to_vec(),
            },
        ),
        _ => encode(
            wire_id::CONNECTION_STATISTICS_RES,
            &WireConnectionStatisticsV3 {
                directory_number: WireAlignedText::new(
                    wire_id::CONNECTION_STATISTICS_RES,
                    "directory number",
                    &statistics.directory_number,
                )?,
                call_reference: statistics.call_reference,
                processing: statistics.processing.wire_value(),
                statistics: tail,
                quality: statistics.quality.as_bytes().to_vec(),
            },
        ),
    }
}

fn encode_start_media_ack(
    ack: &MediaTransmissionAck,
    protocol: ProtocolVersion,
) -> Result<Vec<u8>, CodecError> {
    match protocol.wire() {
        17.. => {
            let base = WireStartMediaAckV17 {
                conference_id: ack.conference_id,
                passthrough_party_id: ack.passthrough_party_id,
                call_reference: ack.call_reference,
                address: WireExtendedAddress::from_ip(ack.address),
                port: u32::from(ack.port),
                status: ack.status.wire_value(),
            };
            match ack.wire.as_ref().and_then(|wire| wire.extension) {
                Some(extension) => encode(
                    wire_id::START_MEDIA_TRANSMISSION_ACK,
                    &WireStartMediaAckV20 { base, extension },
                ),
                None => encode(wire_id::START_MEDIA_TRANSMISSION_ACK, &base),
            }
        }
        _ => encode(
            wire_id::START_MEDIA_TRANSMISSION_ACK,
            &WireStartMediaAckV3 {
                conference_id: ack.conference_id,
                passthrough_party_id: ack.passthrough_party_id,
                call_reference: ack.call_reference,
                address: WireIpv4Address::from_ip(
                    ack.address,
                    wire_id::START_MEDIA_TRANSMISSION_ACK,
                    "IP address family for this protocol version",
                )?,
                port: u32::from(ack.port),
                status: ack.status.wire_value(),
            },
        ),
    }
}

impl ServerMessage {
    /// Decode a server-to-phone message using the negotiated version for
    /// layouts whose frame header is zero or otherwise ambiguous.
    pub fn decode(frame: Frame, protocol: ProtocolVersion) -> Result<Self, CodecError> {
        ensure_station_route(&frame, MessageRoute::ControlToStation, "control-to-station")?;
        Self::decode_unchecked(frame, protocol)
    }

    fn decode_unchecked(frame: Frame, protocol: ProtocolVersion) -> Result<Self, CodecError> {
        let p = &frame.payload;
        match frame.message_id {
            wire_id::REGISTER_ACK => {
                let value: WireRegisterAck = decode(frame.message_id, p)?;
                validate_zero_payload(&value.alignment, frame.message_id, 2)?;
                let protocol_features = u32::from_le_bytes(value.protocol_features);
                Ok(Self::RegisterAck {
                    keepalive_seconds: value.keepalive_seconds,
                    secondary_keepalive_seconds: value.secondary_keepalive_seconds,
                    protocol: ProtocolVersion::negotiate(u32::from(value.protocol_features[0]))?,
                    features: PhoneFeatures::from_bits_retain(protocol_features & !0xff),
                    date_template: DateTemplate::new(
                        std::str::from_utf8(
                            &value.date_template[..value
                                .date_template
                                .iter()
                                .position(|byte| *byte == 0)
                                .unwrap_or(6)],
                        )
                        .map_err(|_| CodecError::InvalidText)?,
                    )?,
                })
            }
            wire_id::REGISTER_REJECT => {
                let value: WireFixedText<33> = decode_zero_padded(frame.message_id, p)?;
                Ok(Self::RegisterReject {
                    reason: value.text()?,
                })
            }
            wire_id::KEEP_ALIVE_ACK => Ok(Self::KeepAliveAck),
            wire_id::UNREGISTER_ACK => {
                let _: WireOneWord = decode(frame.message_id, p)?;
                Ok(Self::UnregisterAck)
            }
            wire_id::CAPABILITIES_REQ => Ok(Self::CapabilitiesRequest),
            wire_id::ENUNCIATOR_COMMAND => {
                validate_exact_payload(p, frame.message_id, 0)?;
                Ok(Self::EnunciatorCommand)
            }
            wire_id::CONFIG_STAT => {
                let value: WireConfigStatus = decode(frame.message_id, p)?;
                Ok(Self::ConfigStatus(ConfigurationStatus {
                    device_name: value.device_id.text()?,
                    station_user_id: value.station_user_id,
                    station_instance: value.station_instance,
                    user_name: value.user_name.text()?,
                    server_name: value.server_name.text()?,
                    line_count: value.line_count,
                    speed_dial_count: value.speed_dial_count,
                }))
            }
            wire_id::CONFIG_STAT_DYNAMIC => decode_dynamic_config_status(p),
            wire_id::LINE_STAT => {
                let value: WireLineStatus = decode(frame.message_id, p)?;
                Ok(Self::LineStatus {
                    instance: value.line_instance,
                    directory_number: value.directory_number.text()?,
                    fully_qualified_display_name: value.display_name.text()?,
                    display_label: value.display_label.text()?,
                })
            }
            wire_id::LINE_STAT_DYNAMIC => decode_dynamic_line_status(p),
            wire_id::BUTTON_TEMPLATE => {
                let value: WireButtonTemplate = decode(frame.message_id, p)?;
                if value.count > BUTTON_TEMPLATE_ENTRIES_PER_CHUNK as u32 {
                    return Err(CodecError::CountTooLarge {
                        message_id: frame.message_id,
                        field: "button definitions in message",
                        count: usize_from_wire(
                            frame.message_id,
                            "button definitions in message",
                            value.count,
                        )?,
                        maximum: BUTTON_TEMPLATE_ENTRIES_PER_CHUNK,
                    });
                }
                let total = usize_from_wire(frame.message_id, "button definitions", value.total)?;
                let offset = usize_from_wire(frame.message_id, "button offset", value.offset)?;
                let count = usize_from_wire(frame.message_id, "button definitions", value.count)?;
                if offset.checked_add(count).is_none_or(|end| end > total) {
                    return Err(CodecError::InvalidValue {
                        message_id: frame.message_id,
                        field: "button template range",
                        value: u64::from(value.offset) + u64::from(value.count),
                    });
                }
                let buttons = value.definitions[..count]
                    .iter()
                    .map(|definition| ButtonTemplateEntry {
                        instance: u32::from(definition.instance),
                        button_type: ButtonType::from(u32::from(definition.button_type)),
                    })
                    .collect::<Vec<_>>();
                Ok(Self::ButtonTemplate {
                    offset: value.offset,
                    total: value.total,
                    buttons,
                })
            }
            wire_id::VERSION => {
                let value: WireFixedText<16> = decode(frame.message_id, p)?;
                Ok(Self::Version {
                    firmware: value.text()?,
                })
            }
            wire_id::SERVER_RES => {
                let servers = match protocol.wire() {
                    17.. => {
                        let value: WireServerResponse<WireExtendedAddress> =
                            decode(frame.message_id, p)?;
                        decode_server_endpoints(
                            frame.message_id,
                            value.names,
                            value.ports,
                            value
                                .addresses
                                .map(|address| address.to_ip(frame.message_id))
                                .into_iter()
                                .collect::<Result<Vec<_>, _>>()?,
                        )?
                    }
                    _ => {
                        let value: WireServerResponse<WireIpv4Address> =
                            decode(frame.message_id, p)?;
                        decode_server_endpoints(
                            frame.message_id,
                            value.names,
                            value.ports,
                            value
                                .addresses
                                .map(|address| address.to_ip(frame.message_id))
                                .into_iter()
                                .collect::<Result<Vec<_>, _>>()?,
                        )?
                    }
                };
                Ok(Self::ServerResponse { servers })
            }
            wire_id::DEFINE_TIME_DATE => {
                let value: WireTimeDate = decode(frame.message_id, p)?;
                Ok(Self::TimeDate {
                    year: value.year,
                    month: value.month,
                    weekday: value.weekday,
                    day: value.day,
                    hour: value.hour,
                    minute: value.minute,
                    second: value.second,
                    milliseconds: value.milliseconds,
                    unix_seconds: value.unix_seconds,
                })
            }
            wire_id::SOFT_KEY_TEMPLATE_RES => {
                let value: WireSoftKeyTemplate = decode(frame.message_id, p)?;
                let actions = value
                    .definitions
                    .iter()
                    .filter(|definition| definition.event != 0)
                    .map(|definition| SoftKey::from(definition.event))
                    .collect();
                Ok(Self::SoftKeyTemplate { actions })
            }
            wire_id::SOFT_KEY_SET_RES => {
                let value: WireSoftKeySet = decode(frame.message_id, p)?;
                let profile =
                    SoftKeyProfile::new(KeyMode::ALL_KNOWN.iter().copied().map(|mode| {
                        let actions = value
                            .sets
                            .get(mode.wire_value() as usize)
                            .map(|set| {
                                set.template_indexes
                                    .iter()
                                    .copied()
                                    .take_while(|index| *index != 0)
                                    .map(|index| SoftKey::from(u32::from(index)))
                                    .collect()
                            })
                            .unwrap_or_default();
                        (mode, actions)
                    }))?;
                Ok(Self::SoftKeySet { profile })
            }
            wire_id::SELECT_SOFT_KEYS => {
                let value: WireSelectSoftKeys = decode(frame.message_id, p)?;
                Ok(Self::SelectSoftKeys {
                    line_instance: value.line_instance,
                    call_reference: value.call_reference,
                    set: KeyMode::from(value.set),
                    valid_mask: value.valid_mask,
                })
            }
            wire_id::CALL_STATE => {
                let value: WireCallState = decode(frame.message_id, p)?;
                Ok(Self::CallState {
                    state: CallState::from(value.state),
                    line_instance: value.line_instance,
                    call_reference: value.call_reference,
                })
            }
            wire_id::CALL_INFO => {
                let value: WireCallInfo = decode(frame.message_id, p)?;
                let call_type = super::values::CallType::from(value.call_type);
                Ok(Self::CallInfo {
                    info: CallInfo {
                        direction: match call_type {
                            super::values::CallType::Inbound => {
                                crate::types::CallDirection::Inbound
                            }
                            _ => crate::types::CallDirection::Outbound,
                        },
                        calling_name: value.calling_name.text()?,
                        calling_number: value.calling_number.text()?,
                        called_name: value.called_name.text()?,
                        called_number: value.called_number.text()?,
                        original_called_name: value.original_called_name.text()?,
                        original_called_number: value.original_called_number.text()?,
                        last_redirecting_name: value.last_redirecting_name.text()?,
                        last_redirecting_number: value.last_redirecting_number.text()?,
                        original_redirect_reason: value.original_redirect_reason,
                        last_redirect_reason: value.last_redirect_reason,
                        party_restrictions: value.party_restrictions,
                    },
                    line_instance: value.line_instance,
                    call_reference: value.call_reference,
                })
            }
            wire_id::CALL_INFO_DYNAMIC => decode_dynamic_call_info(p, protocol),
            wire_id::DISPLAY_PROMPT_STATUS => {
                let value: WirePromptStatus = decode(frame.message_id, p)?;
                Ok(Self::DisplayPrompt {
                    timeout_seconds: value.timeout_seconds,
                    text: value.text.text()?,
                    line_instance: value.line_instance,
                    call_reference: value.call_reference,
                })
            }
            wire_id::DISPLAY_DYNAMIC_PROMPT_STATUS => {
                const HEADER_SIZE: usize = 12;
                if p.len() < HEADER_SIZE {
                    return Err(CodecError::Truncated {
                        message_id: frame.message_id,
                        needed: HEADER_SIZE,
                        actual: p.len(),
                    });
                }
                let value: WireDynamicPromptHeader = decode(frame.message_id, &p[..HEADER_SIZE])?;
                Ok(Self::DisplayPrompt {
                    timeout_seconds: value.timeout_seconds,
                    text: decode_dynamic_text(frame.message_id, p, HEADER_SIZE)?,
                    line_instance: value.line_instance,
                    call_reference: value.call_reference,
                })
            }
            wire_id::CLEAR_PROMPT_STATUS => {
                let value: WireLineCall = decode(frame.message_id, p)?;
                Ok(Self::ClearPrompt {
                    line_instance: value.line_instance,
                    call_reference: value.call_reference,
                })
            }
            wire_id::DISPLAY_NOTIFY => {
                let value: WireNotify = decode(frame.message_id, p)?;
                Ok(Self::DisplayNotify {
                    timeout_seconds: value.timeout_seconds,
                    text: value.text.text()?,
                })
            }
            wire_id::DISPLAY_DYNAMIC_NOTIFY => {
                const HEADER_SIZE: usize = 4;
                if p.len() < HEADER_SIZE {
                    return Err(CodecError::Truncated {
                        message_id: frame.message_id,
                        needed: HEADER_SIZE,
                        actual: p.len(),
                    });
                }
                let value: WireDynamicNotifyHeader = decode(frame.message_id, &p[..HEADER_SIZE])?;
                Ok(Self::DisplayNotify {
                    timeout_seconds: value.timeout_seconds,
                    text: decode_dynamic_text(frame.message_id, p, HEADER_SIZE)?,
                })
            }
            wire_id::CLEAR_NOTIFY => Ok(Self::ClearNotify),
            wire_id::DISPLAY_PRIORITY_NOTIFY => {
                let value: WirePriorityNotify = decode(frame.message_id, p)?;
                Ok(Self::DisplayPriorityNotify {
                    timeout_seconds: value.timeout_seconds,
                    priority: NotificationPriority::from(value.priority),
                    text: value.text.text()?,
                })
            }
            wire_id::DISPLAY_DYNAMIC_PRIORITY_NOTIFY => {
                const HEADER_SIZE: usize = 8;
                if p.len() < HEADER_SIZE {
                    return Err(CodecError::Truncated {
                        message_id: frame.message_id,
                        needed: HEADER_SIZE,
                        actual: p.len(),
                    });
                }
                let value: WireDynamicPriorityNotifyHeader =
                    decode(frame.message_id, &p[..HEADER_SIZE])?;
                Ok(Self::DisplayPriorityNotify {
                    timeout_seconds: value.timeout_seconds,
                    priority: NotificationPriority::from(value.priority),
                    text: decode_dynamic_text(frame.message_id, p, HEADER_SIZE)?,
                })
            }
            wire_id::CLEAR_PRIORITY_NOTIFY => {
                let value: WireOneWord = decode(frame.message_id, p)?;
                Ok(Self::ClearPriorityNotify {
                    priority: NotificationPriority::from(value.value),
                })
            }
            wire_id::NOTIFY_DTMF_TONE | wire_id::SEND_DTMF_TONE => {
                let value: WireDtmfToneControl = decode(frame.message_id, p)?;
                let message = DtmfToneControl {
                    tone: Tone::from(value.tone),
                    conference_id: value.conference_id.into(),
                    passthrough_party_id: value.passthrough_party_id,
                };
                if frame.message_id == wire_id::NOTIFY_DTMF_TONE {
                    Ok(Self::NotifyDtmfTone(message))
                } else {
                    Ok(Self::SendDtmfTone(message))
                }
            }
            wire_id::START_ANNOUNCEMENT => {
                const PAYLOAD_SIZE: usize = 464;
                validate_exact_payload(p, frame.message_id, PAYLOAD_SIZE)?;
                let value: WireStartAnnouncement = decode(frame.message_id, p)?;
                let mut announcements = value
                    .announcements
                    .into_iter()
                    .map(|entry| AnnouncementEntry {
                        locale: entry.locale,
                        country: entry.country,
                        tone: Tone::from(entry.tone),
                    })
                    .collect::<Vec<_>>();
                while announcements.last().is_some_and(|entry| {
                    entry.locale == 0 && entry.country == 0 && entry.tone.wire_value() == 0
                }) {
                    announcements.pop();
                }
                let mut matrix_conference_party_ids = value.matrix_conference_party_ids.to_vec();
                while matrix_conference_party_ids.last() == Some(&0) {
                    matrix_conference_party_ids.pop();
                }
                Ok(Self::StartAnnouncement {
                    announcements,
                    end_of_ack: value.end_of_ack,
                    conference_id: value.conference_id,
                    matrix_conference_party_ids,
                    hearing_conference_party_mask: value.hearing_conference_party_mask,
                    play_mode: value.play_mode,
                })
            }
            wire_id::STOP_ANNOUNCEMENT => {
                validate_exact_payload(p, frame.message_id, 4)?;
                let value: WireOneWord = decode(frame.message_id, p)?;
                Ok(Self::StopAnnouncement {
                    conference_id: value.value,
                })
            }
            wire_id::ANNOUNCEMENT_FINISH => {
                validate_exact_payload(p, frame.message_id, 8)?;
                let value: WireAnnouncementFinish = decode(frame.message_id, p)?;
                Ok(Self::AnnouncementFinish {
                    conference_id: value.conference_id,
                    play_status: value.play_status,
                })
            }
            wire_id::CLEAR_CONFERENCE => {
                validate_exact_payload(p, frame.message_id, 8)?;
                let value: WireCallParty = decode(frame.message_id, p)?;
                Ok(Self::ClearConference {
                    conference_id: value.call_reference.into(),
                    service_number: value.passthrough_party_id,
                })
            }
            wire_id::CREATE_CONFERENCE_REQ => {
                validate_conference_data_length(p, frame.message_id, 76, 72)?;
                let value: WireCreateConferenceRequest = decode_zero_padded(frame.message_id, p)?;
                Ok(Self::CreateConferenceRequest(CreateConferenceRequest {
                    conference_id: value.conference_id.into(),
                    reserved_participants: value.reserved_participants,
                    resource_type: ConferenceResourceType::from(value.resource_type),
                    application_id: value.application_id.into(),
                    application_conference_id: value.application_conference_id.text()?,
                    application_data: value.application_data.text()?,
                    passthrough_data: value.passthrough_data,
                }))
            }
            wire_id::DELETE_CONFERENCE_REQ => {
                validate_exact_payload(p, frame.message_id, 4)?;
                let value: WireOneWord = decode(frame.message_id, p)?;
                Ok(Self::DeleteConferenceRequest {
                    conference_id: value.value.into(),
                })
            }
            wire_id::MODIFY_CONFERENCE_REQ => {
                validate_conference_data_length(p, frame.message_id, 72, 68)?;
                let value: WireModifyConferenceRequest = decode_zero_padded(frame.message_id, p)?;
                Ok(Self::ModifyConferenceRequest(ModifyConferenceRequest {
                    conference_id: value.conference_id.into(),
                    reserved_participants: value.reserved_participants,
                    application_id: value.application_id.into(),
                    application_conference_id: value.application_conference_id.text()?,
                    application_data: value.application_data.text()?,
                    passthrough_data: value.passthrough_data,
                }))
            }
            wire_id::AUDIT_CONFERENCE_REQ => {
                validate_exact_payload(p, frame.message_id, 0)?;
                Ok(Self::AuditConferenceRequest)
            }
            wire_id::ADD_PARTICIPANT_REQ => {
                let (conference_id, participant) = decode_participant_request(p, frame.message_id)?;
                Ok(Self::AddParticipantRequest(AddParticipantRequest {
                    conference_id,
                    participant,
                }))
            }
            wire_id::DROP_PARTICIPANT_REQ => {
                validate_exact_payload(p, frame.message_id, 8)?;
                let value: WireCallParty = decode(frame.message_id, p)?;
                Ok(Self::DropParticipantRequest {
                    conference_id: value.call_reference.into(),
                    call_reference: value.passthrough_party_id.into(),
                })
            }
            wire_id::AUDIT_PARTICIPANT_REQ => {
                validate_exact_payload(p, frame.message_id, 4)?;
                let value: WireOneWord = decode(frame.message_id, p)?;
                Ok(Self::AuditParticipantRequest {
                    conference_id: value.value.into(),
                })
            }
            wire_id::CHANGE_PARTICIPANT_REQ => {
                let (conference_id, participant) = decode_participant_request(p, frame.message_id)?;
                Ok(Self::ChangeParticipantRequest(ChangeParticipantRequest {
                    conference_id,
                    participant,
                }))
            }
            wire_id::STOP_MULTIMEDIA_TRANSMISSION | wire_id::CLOSE_MULTIMEDIA_RECEIVE_CHANNEL => {
                let value: WireMultimediaStreamControl = decode(frame.message_id, p)?;
                let message = MultimediaStreamControl {
                    conference_id: value.conference_id.into(),
                    passthrough_party_id: value.passthrough_party_id.into(),
                    call_reference: value.call_reference.into(),
                    port_handling_flag: value.port_handling_flag,
                };
                if frame.message_id == wire_id::STOP_MULTIMEDIA_TRANSMISSION {
                    Ok(Self::StopMultimediaTransmission(message))
                } else {
                    Ok(Self::CloseMultimediaReceiveChannel(message))
                }
            }
            wire_id::FLOW_CONTROL_COMMAND | wire_id::FLOW_CONTROL_NOTIFY => {
                let value: WireVideoFlowControl = decode(frame.message_id, p)?;
                let message = VideoFlowControl {
                    conference_id: value.conference_id.into(),
                    passthrough_party_id: value.passthrough_party_id.into(),
                    call_reference: value.call_reference.into(),
                    maximum_bit_rate: value.maximum_bit_rate,
                };
                if frame.message_id == wire_id::FLOW_CONTROL_COMMAND {
                    Ok(Self::FlowControlCommand(message))
                } else {
                    Ok(Self::FlowControlNotify(message))
                }
            }
            wire_id::VIDEO_DISPLAY_COMMAND => {
                let value: WireVideoDisplayCommand = decode(frame.message_id, p)?;
                Ok(Self::VideoDisplayCommand {
                    conference_id: value.conference_id.into(),
                    call_reference: value.call_reference.into(),
                    layout_id: value.layout_id,
                })
            }
            wire_id::ACTIVATE_CALL_PLANE => {
                let value: WireOneWord = decode(frame.message_id, p)?;
                Ok(Self::ActivateCallPlane {
                    line_instance: value.value,
                })
            }
            wire_id::DEACTIVATE_CALL_PLANE => Ok(Self::DeactivateCallPlane),
            wire_id::BACKSPACE_RESPONSE => {
                let value: WireLineCall = decode(frame.message_id, p)?;
                Ok(Self::BackspaceResponse {
                    line_instance: value.line_instance,
                    call_reference: value.call_reference,
                })
            }
            wire_id::REGISTER_TOKEN_ACK => Ok(Self::RegisterTokenAck),
            wire_id::REGISTER_TOKEN_REJECT => {
                let value: WireOneWord = decode(frame.message_id, p)?;
                Ok(Self::RegisterTokenReject {
                    backoff_seconds: value.value,
                })
            }
            wire_id::SPCP_REGISTER_TOKEN_ACK => {
                let value: WireOneWord = decode(frame.message_id, p)?;
                Ok(Self::SpcpRegisterTokenAck {
                    features: value.value,
                })
            }
            wire_id::SPCP_REGISTER_TOKEN_REJECT => {
                let value: WireOneWord = decode(frame.message_id, p)?;
                Ok(Self::SpcpRegisterTokenReject {
                    backoff_seconds: value.value,
                })
            }
            wire_id::SET_RINGER => {
                let value: WireModeLineCall = decode(frame.message_id, p)?;
                Ok(Self::SetRinger {
                    mode: RingerMode::from(value.mode),
                    duration: RingDuration::from(value.duration),
                    line_instance: value.line_instance,
                    call_reference: value.call_reference,
                })
            }
            wire_id::SET_LAMP => {
                let value: WireLampState = decode(frame.message_id, p)?;
                Ok(Self::SetLamp {
                    stimulus: ButtonType::from(value.stimulus),
                    instance: value.instance,
                    mode: LampMode::from(value.mode),
                })
            }
            wire_id::SET_HOOK_FLASH_DETECT => {
                validate_exact_payload(p, frame.message_id, 0)?;
                Ok(Self::SetHookFlashDetect)
            }
            wire_id::START_TONE => {
                let value: WireToneLineCall = decode(frame.message_id, p)?;
                Ok(Self::StartTone {
                    tone: Tone::from(value.tone),
                    direction: ToneDirection::from(value.direction),
                    line_instance: value.line_instance,
                    call_reference: value.call_reference,
                })
            }
            wire_id::STOP_TONE => {
                let (line_instance, call_reference) = match protocol.wire() {
                    12.. => {
                        let value: WireStopToneV12 = decode(frame.message_id, p)?;
                        (value.line_instance, value.call_reference)
                    }
                    _ => {
                        let value: WireLineCall = decode(frame.message_id, p)?;
                        (value.line_instance, value.call_reference)
                    }
                };
                Ok(Self::StopTone {
                    line_instance,
                    call_reference,
                })
            }
            wire_id::START_MULTICAST_MEDIA_RECEPTION => {
                decode_start_multicast_reception(p, protocol, frame.message_id)
            }
            wire_id::START_MULTICAST_MEDIA_TRANSMISSION => {
                decode_start_multicast_transmission(p, protocol, frame.message_id)
            }
            wire_id::STOP_MULTICAST_MEDIA_RECEPTION
            | wire_id::STOP_MULTICAST_MEDIA_TRANSMISSION => {
                validate_exact_payload(p, frame.message_id, 12)?;
                let value: WireStopMulticast = decode(frame.message_id, p)?;
                if frame.message_id == wire_id::STOP_MULTICAST_MEDIA_RECEPTION {
                    Ok(Self::StopMulticastMediaReception {
                        conference_id: value.conference_id.into(),
                        passthrough_party_id: value.passthrough_party_id.into(),
                        call_reference: value.call_reference.into(),
                    })
                } else {
                    Ok(Self::StopMulticastMediaTransmission {
                        conference_id: value.conference_id.into(),
                        passthrough_party_id: value.passthrough_party_id.into(),
                        call_reference: value.call_reference.into(),
                    })
                }
            }
            wire_id::OPEN_RECEIVE_CHANNEL => decode_open_receive(p, protocol, frame.message_id),
            wire_id::CLOSE_RECEIVE_CHANNEL => {
                validate_exact_payload(p, frame.message_id, 16)?;
                let value: WireAudioStreamControl = decode(frame.message_id, p)?;
                Ok(Self::CloseReceiveChannel(AudioStreamControl {
                    conference_id: value.conference_id.into(),
                    passthrough_party_id: value.passthrough_party_id.into(),
                    call_reference: value.call_reference.into(),
                    port_handling_flag: value.port_handling_flag,
                }))
            }
            wire_id::CONNECTION_STATISTICS_REQ => match protocol.wire() {
                19.. => decode_connection_statistics_request::<25, 3>(p, frame.message_id),
                _ => decode_connection_statistics_request::<24, 0>(p, frame.message_id),
            },
            wire_id::START_MEDIA_TRANSMISSION => decode_start_media(p, protocol, frame.message_id),
            wire_id::STOP_MEDIA_TRANSMISSION => {
                validate_exact_payload(p, frame.message_id, 16)?;
                let value: WireAudioStreamControl = decode(frame.message_id, p)?;
                Ok(Self::StopMediaTransmission(AudioStreamControl {
                    conference_id: value.conference_id.into(),
                    passthrough_party_id: value.passthrough_party_id.into(),
                    call_reference: value.call_reference.into(),
                    port_handling_flag: value.port_handling_flag,
                }))
            }
            wire_id::START_MEDIA_RECEPTION => {
                validate_exact_payload(p, frame.message_id, 0)?;
                Ok(Self::StartMediaReception)
            }
            wire_id::STOP_MEDIA_RECEPTION => {
                let value: WireStopMediaReception = decode(frame.message_id, p)?;
                Ok(Self::StopMediaReception {
                    conference_id: value.conference_id.into(),
                    passthrough_party_id: value.passthrough_party_id.into(),
                })
            }
            wire_id::SET_SPEAKER_MODE => {
                let value: WireOneWord = decode(frame.message_id, p)?;
                Ok(Self::SetSpeakerMode(SpeakerMode::from(value.value)))
            }
            wire_id::SET_MICROPHONE_MODE => {
                let value: WireOneWord = decode(frame.message_id, p)?;
                Ok(Self::SetMicrophoneMode(MicrophoneMode::from(value.value)))
            }
            wire_id::RESET => {
                let value: WireOneWord = decode(frame.message_id, p)?;
                Ok(Self::Reset(ResetType::from(value.value)))
            }
            wire_id::DISPLAY_TEXT => {
                let value: WireFixedText<32> = decode(frame.message_id, p)?;
                Ok(Self::DisplayText {
                    text: value.text()?,
                })
            }
            wire_id::CLEAR_DISPLAY => Ok(Self::ClearDisplay),
            wire_id::FORWARD_STAT => match protocol.wire() {
                19.. => decode_forward_status::<25, 3>(p, frame.message_id),
                _ => decode_forward_status::<24, 0>(p, frame.message_id),
            },
            wire_id::SPEED_DIAL_STAT => {
                let value: WireSpeedDialStatus = decode(frame.message_id, p)?;
                Ok(Self::SpeedDialStatus {
                    instance: value.instance,
                    number: value.number.text()?,
                    display_name: value.display_name.text()?,
                })
            }
            wire_id::SPEED_DIAL_STAT_DYNAMIC => decode_dynamic_speed_dial_status(p),
            wire_id::START_MEDIA_FAILURE_DETECTION => {
                let value: WireMediaFailureDetection = decode(frame.message_id, p)?;
                Ok(Self::StartMediaFailureDetection(MediaFailureDetection {
                    conference_id: value.conference_id.into(),
                    passthrough_party_id: value.passthrough_party_id,
                    packet_millis: value.packet_millis,
                    codec: Codec::from(value.codec),
                    echo_cancellation: EchoCancellation::from(value.echo_cancellation),
                    codec_qualifier: value.codec_qualifier,
                    call_reference: value.call_reference.into(),
                }))
            }
            wire_id::OPEN_MULTIMEDIA_CHANNEL => {
                decode_open_multimedia(p, protocol, frame.message_id)
                    .map(Self::OpenMultimediaChannel)
            }
            wire_id::START_MULTIMEDIA_TRANSMISSION => {
                decode_start_multimedia(p, protocol, frame.message_id)
                    .map(Self::StartMultimediaTransmission)
            }
            wire_id::MISCELLANEOUS_COMMAND => {
                decode_miscellaneous_command(p, frame.message_id).map(Self::MiscellaneousCommand)
            }
            wire_id::DIALED_NUMBER => match protocol.wire() {
                19.. => decode_dialed_number::<25, 3>(p, frame.message_id),
                _ => decode_dialed_number::<24, 0>(p, frame.message_id),
            },
            wire_id::SUBSCRIBE_DTMF_PAYLOAD_REQ => {
                let value: WireDtmfPayloadRequest = decode(frame.message_id, p)?;
                Ok(Self::SubscribeDtmfPayloadRequest(
                    dtmf_payload_request_from_wire(value),
                ))
            }
            wire_id::SUBSCRIBE_DTMF_PAYLOAD_ERR => {
                let value: WireDtmfPayloadIdentity = decode(frame.message_id, p)?;
                Ok(Self::SubscribeDtmfPayloadError(
                    dtmf_payload_identity_from_wire(value),
                ))
            }
            wire_id::UNSUBSCRIBE_DTMF_PAYLOAD_REQ => {
                let value: WireDtmfPayloadRequest = decode(frame.message_id, p)?;
                Ok(Self::UnsubscribeDtmfPayloadRequest(
                    dtmf_payload_request_from_wire(value),
                ))
            }
            wire_id::UNSUBSCRIBE_DTMF_PAYLOAD_ERR => {
                let value: WireDtmfPayloadIdentity = decode(frame.message_id, p)?;
                Ok(Self::UnsubscribeDtmfPayloadError(
                    dtmf_payload_identity_from_wire(value),
                ))
            }
            wire_id::USER_TO_DEVICE_DATA => {
                decode_user_data(p, frame.message_id).map(Self::UserToDeviceData)
            }
            wire_id::USER_TO_DEVICE_DATA_V1 => {
                decode_user_data_v1(p, frame.message_id).map(Self::UserToDeviceDataV1)
            }
            wire_id::FEATURE_STAT => {
                let value: WireFeatureStatus = decode(frame.message_id, p)?;
                Ok(Self::FeatureStatus {
                    instance: value.instance,
                    button_type: ButtonType::from(value.button_type),
                    label: value.label.text()?,
                    state: value.state,
                })
            }
            wire_id::FEATURE_STAT_DYNAMIC => {
                let value: WireFeatureStatusDynamic = decode(frame.message_id, p)?;
                Ok(Self::FeatureStatus {
                    instance: value.instance,
                    button_type: ButtonType::from(value.button_type),
                    label: value.label.text()?,
                    state: value.state,
                })
            }
            wire_id::SERVICE_URL_STAT => {
                let value: WireServiceUrlStatus = decode(frame.message_id, p)?;
                Ok(Self::ServiceUrlStatus {
                    index: value.index,
                    url: value.url.text()?,
                    label: value.label.text()?,
                    extension_text: String::new(),
                })
            }
            wire_id::SERVICE_URL_STAT_DYNAMIC => decode_dynamic_service_url_status(p, protocol),
            wire_id::CALL_SELECT_STAT => {
                let value: WireCallSelectStatus = decode(frame.message_id, p)?;
                Ok(Self::CallSelectStatus {
                    status: value.status,
                    call_reference: value.call_reference,
                    line_instance: value.line_instance,
                })
            }
            wire_id::PORT_REQUEST => {
                let request = match protocol.wire() {
                    20.. => {
                        let value: WirePortRequestV20 = decode(frame.message_id, p)?;
                        PortRequest {
                            conference_id: value.base.conference_id.into(),
                            call_reference: value.base.call_reference.into(),
                            passthrough_party_id: value.base.passthrough_party_id.into(),
                            transport: MediaTransport::from(value.base.transport),
                            address_type: Some(IpAddressType::from(value.address_type)),
                            media_type: Some(MediaType::from(value.media_type)),
                        }
                    }
                    _ => {
                        let value: WirePortRequest = decode(frame.message_id, p)?;
                        PortRequest {
                            conference_id: value.conference_id.into(),
                            call_reference: value.call_reference.into(),
                            passthrough_party_id: value.passthrough_party_id.into(),
                            transport: MediaTransport::from(value.transport),
                            address_type: None,
                            media_type: None,
                        }
                    }
                };
                Ok(Self::PortRequest(request))
            }
            wire_id::PORT_CLOSE => {
                let close = match protocol.wire() {
                    20.. => {
                        let value: WirePortCloseV20 = decode(frame.message_id, p)?;
                        PortClose {
                            conference_id: value.base.conference_id.into(),
                            call_reference: value.base.call_reference.into(),
                            passthrough_party_id: value.base.passthrough_party_id.into(),
                            media_type: Some(MediaType::from(value.media_type)),
                        }
                    }
                    _ => {
                        let value: WirePortClose = decode(frame.message_id, p)?;
                        PortClose {
                            conference_id: value.conference_id.into(),
                            call_reference: value.call_reference.into(),
                            passthrough_party_id: value.passthrough_party_id.into(),
                            media_type: None,
                        }
                    }
                };
                Ok(Self::PortClose(close))
            }
            wire_id::SUBSCRIPTION_STAT => {
                let value: WireSubscriptionStatus = decode(frame.message_id, p)?;
                Ok(Self::SubscriptionStatus {
                    transaction_id: value.transaction_id,
                    feature_id: value.feature_id,
                    timer_seconds: value.timer_seconds,
                    cause: SubscriptionCause::from(value.cause),
                })
            }
            wire_id::NOTIFICATION => {
                let value: WireNotification = decode(frame.message_id, p)?;
                Ok(Self::Notification {
                    transaction_id: value.transaction_id,
                    feature_id: value.feature_id,
                    status: BusyLampFieldState::from(value.status),
                    text: value.text.text()?,
                })
            }
            wire_id::CALL_HISTORY_DISPOSITION => {
                let value: WireCallHistoryDisposition = decode(frame.message_id, p)?;
                Ok(Self::CallHistoryDisposition {
                    disposition: CallHistoryDisposition::from(value.disposition),
                    line_instance: value.line_instance,
                    call_reference: value.call_reference,
                })
            }
            wire_id::CALL_COUNT_RES => {
                let value: WireCallCountResponse = decode(frame.message_id, p)?;
                let line_data_entries = usize_from_wire(
                    frame.message_id,
                    "call-count line data",
                    value.line_data_entries,
                )?;
                if line_data_entries > CALL_COUNT_RESPONSE_MAX_LINE_ENTRIES {
                    return Err(CodecError::CountTooLarge {
                        message_id: frame.message_id,
                        field: "call-count line data",
                        count: line_data_entries,
                        maximum: CALL_COUNT_RESPONSE_MAX_LINE_ENTRIES,
                    });
                }
                Ok(Self::CallCountResponse(CallCountResponse {
                    total_configured_lines: value.total_configured_lines,
                    starting_line_instance: value.starting_line_instance,
                    line_data: value
                        .line_data
                        .into_iter()
                        .take(line_data_entries)
                        .map(|entry| CallCountLineData {
                            max_calls: entry.max_calls,
                            busy_trigger: entry.busy_trigger,
                        })
                        .collect(),
                }))
            }
            wire_id::RECORDING_STATUS => {
                let value: WireRecordingStatus = decode(frame.message_id, p)?;
                Ok(Self::RecordingStatus {
                    call_reference: value.call_reference,
                    active: decode_bool_word(value.active, frame.message_id, "recording active")?,
                })
            }
            _ => {
                let message_type = frame.message_type();
                if message_type.is_known() {
                    preserve_known_message(frame, message_type).map(Self::KnownOpaque)
                } else {
                    Ok(Self::Unknown(RawMessage {
                        message_id: frame.message_id,
                        protocol_version: frame.protocol_version,
                        payload: frame.payload,
                    }))
                }
            }
        }
    }

    /// Encodes a control-to-station message using version-only layout selection.
    ///
    /// When negotiated feature flags also select layouts, use
    /// [`Self::encode_for_session`].
    pub fn encode(&self, protocol: ProtocolVersion) -> Result<Vec<u8>, CodecError> {
        self.encode_for_session(protocol.into())
    }

    /// Encodes a control-to-station message with complete session layout inputs.
    pub fn encode_for_session(
        &self,
        session: StationSessionContext,
    ) -> Result<Vec<u8>, CodecError> {
        let (message_id, payload, header_protocol) = self.payload(session, None)?;
        reject_non_station_route(
            message_id,
            MessageRoute::ControlToStation,
            "control-to-station",
        )?;
        Frame::new(header_protocol, message_id, payload).encode()
    }

    fn encode_unchecked(&self, protocol: ProtocolVersion) -> Result<Vec<u8>, CodecError> {
        let (message_id, payload, header_protocol) = self.payload(protocol.into(), None)?;
        Frame::new(header_protocol, message_id, payload).encode()
    }

    /// Encode station-facing labels in a legacy single-byte code page.
    ///
    /// Version-only layout selection is used. See
    /// [`Self::encode_for_legacy_session`] when feature flags also matter.
    pub fn encode_for_legacy_station(
        &self,
        protocol: ProtocolVersion,
        code_page: LegacyCodePage,
    ) -> Result<Vec<u8>, CodecError> {
        self.encode_for_legacy_session(protocol.into(), code_page)
    }

    /// Encodes a station message with session-aware layout selection and a
    /// legacy single-byte code page for user-visible labels.
    pub fn encode_for_legacy_session(
        &self,
        session: StationSessionContext,
        code_page: LegacyCodePage,
    ) -> Result<Vec<u8>, CodecError> {
        let (message_id, payload, header_protocol) = self.payload(session, Some(code_page))?;
        reject_non_station_route(
            message_id,
            MessageRoute::ControlToStation,
            "control-to-station",
        )?;
        Frame::new(header_protocol, message_id, payload).encode()
    }

    fn payload(
        &self,
        session: StationSessionContext,
        legacy_code_page: Option<LegacyCodePage>,
    ) -> Result<(u32, Vec<u8>, u32), CodecError> {
        let protocol = session.protocol;
        let mut p = Vec::new();
        let id = match self {
            Self::RegisterAck {
                keepalive_seconds,
                secondary_keepalive_seconds,
                protocol,
                features,
                date_template,
            } => {
                if date_template.as_str().len() > 6 {
                    return Err(CodecError::TextTooLong {
                        message_id: wire_id::REGISTER_ACK,
                        field: "date template",
                        actual: date_template.as_str().len(),
                        maximum: 6,
                    });
                }
                let mut wire_date_template = [0_u8; 6];
                wire_date_template[..date_template.as_str().len()]
                    .copy_from_slice(date_template.as_str().as_bytes());
                p = encode(
                    wire_id::REGISTER_ACK,
                    &WireRegisterAck {
                        keepalive_seconds: *keepalive_seconds,
                        date_template: wire_date_template,
                        alignment: [0; 2],
                        secondary_keepalive_seconds: *secondary_keepalive_seconds,
                        protocol_features: {
                            let mut bytes = features.bits().to_le_bytes();
                            bytes[0] = protocol.wire() as u8;
                            bytes
                        },
                    },
                )?;
                return Ok((wire_id::REGISTER_ACK, p, 0));
            }
            Self::RegisterReject { reason } => {
                p = encode(
                    wire_id::REGISTER_REJECT,
                    &WireFixedText::<33>::new(wire_id::REGISTER_REJECT, "reject reason", reason)?,
                )?;
                pad_dynamic_payload(&mut p);
                wire_id::REGISTER_REJECT
            }
            Self::KeepAliveAck => return Ok((wire_id::KEEP_ALIVE_ACK, p, 0)),
            Self::UnregisterAck => {
                p = encode(wire_id::UNREGISTER_ACK, &WireOneWord { value: 0 })?;
                return Ok((wire_id::UNREGISTER_ACK, p, 0));
            }
            Self::CapabilitiesRequest => wire_id::CAPABILITIES_REQ,
            Self::EnunciatorCommand => wire_id::ENUNCIATOR_COMMAND,
            Self::ConfigStatus(status) => {
                if session.uses_dynamic_general_ui() {
                    p = encode_dynamic_config_status(status)?;
                    wire_id::CONFIG_STAT_DYNAMIC
                } else {
                    p = encode(
                        wire_id::CONFIG_STAT,
                        &WireConfigStatus {
                            device_id: WireFixedText::new(
                                wire_id::CONFIG_STAT,
                                "device ID",
                                &status.device_name,
                            )?,
                            station_user_id: status.station_user_id,
                            station_instance: status.station_instance,
                            user_name: WireFixedText::new_station(
                                wire_id::CONFIG_STAT,
                                "user name",
                                &status.user_name,
                                legacy_code_page,
                            )?,
                            server_name: WireFixedText::new_station(
                                wire_id::CONFIG_STAT,
                                "server name",
                                &status.server_name,
                                legacy_code_page,
                            )?,
                            line_count: status.line_count,
                            speed_dial_count: status.speed_dial_count,
                        },
                    )?;
                    wire_id::CONFIG_STAT
                }
            }
            Self::LineStatus {
                instance,
                directory_number,
                fully_qualified_display_name,
                display_label,
            } => {
                if session.uses_dynamic_general_ui() {
                    p = encode_dynamic_line_status(
                        *instance,
                        directory_number,
                        fully_qualified_display_name,
                        display_label,
                        legacy_code_page,
                    )?;
                    wire_id::LINE_STAT_DYNAMIC
                } else {
                    p = encode(
                        wire_id::LINE_STAT,
                        &WireLineStatus {
                            line_instance: *instance,
                            directory_number: WireFixedText::new(
                                wire_id::LINE_STAT,
                                "line number",
                                directory_number,
                            )?,
                            display_name: WireFixedText::new_station(
                                wire_id::LINE_STAT,
                                "display name",
                                fully_qualified_display_name,
                                legacy_code_page,
                            )?,
                            display_label: WireFixedText::new_station(
                                wire_id::LINE_STAT,
                                "line label",
                                display_label,
                                legacy_code_page,
                            )?,
                            reserved: 0,
                        },
                    )?;
                    wire_id::LINE_STAT
                }
            }
            Self::ButtonTemplate {
                offset,
                total,
                buttons,
            } => {
                if buttons.len() > BUTTON_TEMPLATE_ENTRIES_PER_CHUNK {
                    return Err(CodecError::CountTooLarge {
                        message_id: wire_id::BUTTON_TEMPLATE,
                        field: "button definitions",
                        count: buttons.len(),
                        maximum: BUTTON_TEMPLATE_ENTRIES_PER_CHUNK,
                    });
                }
                let count = u32::try_from(buttons.len()).map_err(|_| CodecError::InvalidValue {
                    message_id: wire_id::BUTTON_TEMPLATE,
                    field: "button definitions in message",
                    value: buttons.len() as u64,
                })?;
                if offset.checked_add(count).is_none_or(|end| end > *total) {
                    return Err(CodecError::InvalidValue {
                        message_id: wire_id::BUTTON_TEMPLATE,
                        field: "button template range",
                        value: u64::from(*offset) + u64::from(count),
                    });
                }
                let mut definitions =
                    [WireButtonDefinition::default(); BUTTON_TEMPLATE_ENTRIES_PER_CHUNK];
                for (index, button) in buttons.iter().enumerate() {
                    definitions[index] = WireButtonDefinition {
                        instance: u8::try_from(button.instance).map_err(|_| {
                            CodecError::InvalidValue {
                                message_id: wire_id::BUTTON_TEMPLATE,
                                field: "button instance",
                                value: u64::from(button.instance),
                            }
                        })?,
                        button_type: u8::try_from(button.button_type.wire_value()).map_err(
                            |_| CodecError::InvalidValue {
                                message_id: wire_id::BUTTON_TEMPLATE,
                                field: "button type",
                                value: u64::from(button.button_type.wire_value()),
                            },
                        )?,
                    };
                }
                p = encode(
                    wire_id::BUTTON_TEMPLATE,
                    &WireButtonTemplate {
                        offset: *offset,
                        count,
                        total: *total,
                        definitions,
                    },
                )?;
                wire_id::BUTTON_TEMPLATE
            }
            Self::Version { firmware } => {
                p = encode(
                    wire_id::VERSION,
                    &WireFixedText::<16>::new(wire_id::VERSION, "firmware", firmware)?,
                )?;
                wire_id::VERSION
            }
            Self::ServerResponse { servers } => {
                if servers.is_empty() {
                    return Err(CodecError::InvalidValue {
                        message_id: wire_id::SERVER_RES,
                        field: "server endpoints",
                        value: 0,
                    });
                }
                if servers.len() > MAX_SIGNALING_SERVERS {
                    return Err(CodecError::CountTooLarge {
                        message_id: wire_id::SERVER_RES,
                        field: "server endpoints",
                        count: servers.len(),
                        maximum: MAX_SIGNALING_SERVERS,
                    });
                }
                if servers
                    .iter()
                    .any(|server| server.address.is_unspecified() || server.address.is_multicast())
                {
                    return Err(CodecError::InvalidValue {
                        message_id: wire_id::SERVER_RES,
                        field: "server address",
                        value: 0,
                    });
                }
                let names: [WireFixedText<48>; MAX_SIGNALING_SERVERS] = (0..MAX_SIGNALING_SERVERS)
                    .map(|index| {
                        WireFixedText::new(
                            wire_id::SERVER_RES,
                            "server name",
                            servers.get(index).map_or("", |server| server.name.as_str()),
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()?
                    .try_into()
                    .map_err(|_| CodecError::InvalidValue {
                        message_id: wire_id::SERVER_RES,
                        field: "server endpoint array",
                        value: servers.len() as u64,
                    })?;
                let ports = std::array::from_fn(|index| {
                    servers
                        .get(index)
                        .map_or(0, |server| u32::from(server.port.get()))
                });
                p = match protocol.wire() {
                    17.. => encode(
                        wire_id::SERVER_RES,
                        &WireServerResponse::<WireExtendedAddress> {
                            names,
                            ports,
                            addresses: std::array::from_fn(|index| {
                                WireExtendedAddress::from_ip(
                                    servers
                                        .get(index)
                                        .map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |server| {
                                            server.address
                                        }),
                                )
                            }),
                        },
                    ),
                    _ => {
                        let addresses: [WireIpv4Address; MAX_SIGNALING_SERVERS] = (0
                            ..MAX_SIGNALING_SERVERS)
                            .map(|index| {
                                WireIpv4Address::from_ip(
                                    servers
                                        .get(index)
                                        .map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |server| {
                                            server.address
                                        }),
                                    wire_id::SERVER_RES,
                                    "IP address family for pre-v17 protocol",
                                )
                            })
                            .collect::<Result<Vec<_>, _>>()?
                            .try_into()
                            .map_err(|_| CodecError::InvalidValue {
                                message_id: wire_id::SERVER_RES,
                                field: "server address array",
                                value: servers.len() as u64,
                            })?;
                        encode(
                            wire_id::SERVER_RES,
                            &WireServerResponse::<WireIpv4Address> {
                                names,
                                ports,
                                addresses,
                            },
                        )
                    }
                }?;
                wire_id::SERVER_RES
            }
            Self::TimeDate {
                year,
                month,
                weekday,
                day,
                hour,
                minute,
                second,
                milliseconds,
                unix_seconds,
            } => {
                p = encode(
                    wire_id::DEFINE_TIME_DATE,
                    &WireTimeDate {
                        year: *year,
                        month: *month,
                        weekday: *weekday,
                        day: *day,
                        hour: *hour,
                        minute: *minute,
                        second: *second,
                        milliseconds: *milliseconds,
                        unix_seconds: *unix_seconds,
                    },
                )?;
                wire_id::DEFINE_TIME_DATE
            }
            Self::SoftKeyTemplate { actions } => {
                // SoftKeyEvent returns the template position, so the canonical
                // 32-entry protocol order must remain stable
                // even when the active set exposes only a subset.
                const LABELS: [u16; 32] = [
                    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 202, 65, 67, 63,
                    79, 78, 54, 62, 77, 80, 88, 60, 0, 201,
                ];
                let mut available = [false; 32];
                for action in actions {
                    let value = action.wire_value();
                    if !action.is_known() || value == 0 || value > available.len() as u32 {
                        return Err(CodecError::InvalidDefinition(format!(
                            "soft-key template contains unknown action {value}"
                        )));
                    }
                    let slot = value as usize - 1;
                    if std::mem::replace(&mut available[slot], true) {
                        return Err(CodecError::InvalidDefinition(format!(
                            "soft-key template repeats action {value}"
                        )));
                    }
                }
                p = encode(
                    wire_id::SOFT_KEY_TEMPLATE_RES,
                    &WireSoftKeyTemplate {
                        offset: 0,
                        count: 32,
                        total: 32,
                        definitions: std::array::from_fn(|index| {
                            if !available[index] {
                                return WireSoftKeyDefinition {
                                    label: [0; 16],
                                    event: 0,
                                };
                            }
                            let label = LABELS[index];
                            let mut encoded = [0; 16];
                            match label {
                                201 => encoded[..4].copy_from_slice(b"Dial"),
                                0 => {}
                                _ => {
                                    encoded[0] = 0x80;
                                    encoded[1] = label as u8;
                                }
                            }
                            WireSoftKeyDefinition {
                                label: encoded,
                                event: index as u32 + 1,
                            }
                        }),
                    },
                )?;
                wire_id::SOFT_KEY_TEMPLATE_RES
            }
            Self::SoftKeySet { profile } => {
                let definitions = (0_u32..16)
                    .map(KeyMode::from)
                    .map(|mode| profile.actions(mode))
                    .map(|actions| {
                        let mut indexes = [0_u8; 16];
                        let mut info = [0_u16; 16];
                        for (slot, action) in actions.iter().copied().enumerate() {
                            let template = action.wire_value() as u8;
                            indexes[slot] = template;
                            info[slot] = u16::from(template) + 300;
                        }
                        WireSoftKeySetDefinition {
                            template_indexes: indexes,
                            info,
                        }
                    })
                    .collect();
                p = encode(
                    wire_id::SOFT_KEY_SET_RES,
                    &WireSoftKeySet {
                        offset: 0,
                        count: 16,
                        total: 16,
                        sets: definitions,
                    },
                )?;
                wire_id::SOFT_KEY_SET_RES
            }
            Self::SelectSoftKeys {
                line_instance,
                call_reference,
                set,
                valid_mask,
            } => {
                p = encode(
                    wire_id::SELECT_SOFT_KEYS,
                    &WireSelectSoftKeys {
                        line_instance: *line_instance,
                        call_reference: *call_reference,
                        set: set.wire_value(),
                        valid_mask: *valid_mask,
                    },
                )?;
                wire_id::SELECT_SOFT_KEYS
            }
            Self::CallState {
                state,
                line_instance,
                call_reference,
            } => {
                p = encode(
                    wire_id::CALL_STATE,
                    &WireCallState {
                        state: state.wire_value(),
                        line_instance: *line_instance,
                        call_reference: *call_reference,
                        visibility: 0,
                        precedence: call_state_precedence(*state),
                        domain: 0,
                    },
                )?;
                wire_id::CALL_STATE
            }
            Self::CallInfo {
                info,
                line_instance,
                call_reference,
            } => {
                if session.uses_dynamic_general_ui() {
                    p = encode_dynamic_call_info(info, *line_instance, *call_reference, protocol)?;
                    wire_id::CALL_INFO_DYNAMIC
                } else {
                    p = encode(
                        wire_id::CALL_INFO,
                        &WireCallInfo {
                            calling_name: WireFixedText::new(
                                wire_id::CALL_INFO,
                                "calling name",
                                &info.calling_name,
                            )?,
                            calling_number: WireFixedText::new(
                                wire_id::CALL_INFO,
                                "calling number",
                                &info.calling_number,
                            )?,
                            called_name: WireFixedText::new(
                                wire_id::CALL_INFO,
                                "called name",
                                &info.called_name,
                            )?,
                            called_number: WireFixedText::new(
                                wire_id::CALL_INFO,
                                "called number",
                                &info.called_number,
                            )?,
                            line_instance: *line_instance,
                            call_reference: *call_reference,
                            call_type: match info.direction {
                                crate::types::CallDirection::Inbound => 1,
                                crate::types::CallDirection::Outbound => 2,
                            },
                            original_called_name: WireFixedText::new(
                                wire_id::CALL_INFO,
                                "original called name",
                                &info.original_called_name,
                            )?,
                            original_called_number: WireFixedText::new(
                                wire_id::CALL_INFO,
                                "original called number",
                                &info.original_called_number,
                            )?,
                            last_redirecting_name: WireFixedText::new(
                                wire_id::CALL_INFO,
                                "last redirecting name",
                                &info.last_redirecting_name,
                            )?,
                            last_redirecting_number: WireFixedText::new(
                                wire_id::CALL_INFO,
                                "last redirecting number",
                                &info.last_redirecting_number,
                            )?,
                            original_redirect_reason: info.original_redirect_reason,
                            last_redirect_reason: info.last_redirect_reason,
                            voice_mailboxes: std::array::from_fn(|_| {
                                WireFixedText::new(wire_id::CALL_INFO, "voice mailbox", "").unwrap()
                            }),
                            call_instance: 1,
                            security_status: 0,
                            party_restrictions: info.party_restrictions,
                        },
                    )?;
                    wire_id::CALL_INFO
                }
            }
            Self::DisplayPrompt {
                timeout_seconds,
                text,
                line_instance,
                call_reference,
            } => {
                if session.uses_dynamic_general_ui() {
                    p = encode(
                        wire_id::DISPLAY_DYNAMIC_PROMPT_STATUS,
                        &WireDynamicPromptHeader {
                            timeout_seconds: *timeout_seconds,
                            line_instance: *line_instance,
                            call_reference: *call_reference,
                        },
                    )?;
                    push_dynamic_text(
                        &mut p,
                        wire_id::DISPLAY_DYNAMIC_PROMPT_STATUS,
                        "prompt",
                        text,
                        96,
                    )?;
                    pad_dynamic_payload(&mut p);
                    wire_id::DISPLAY_DYNAMIC_PROMPT_STATUS
                } else {
                    p = encode(
                        wire_id::DISPLAY_PROMPT_STATUS,
                        &WirePromptStatus {
                            timeout_seconds: *timeout_seconds,
                            text: WireFixedText::new(
                                wire_id::DISPLAY_PROMPT_STATUS,
                                "prompt",
                                text,
                            )?,
                            line_instance: *line_instance,
                            call_reference: *call_reference,
                        },
                    )?;
                    wire_id::DISPLAY_PROMPT_STATUS
                }
            }
            Self::ClearPrompt {
                line_instance,
                call_reference,
            } => {
                p = encode(
                    wire_id::CLEAR_PROMPT_STATUS,
                    &WireLineCall {
                        line_instance: *line_instance,
                        call_reference: *call_reference,
                    },
                )?;
                wire_id::CLEAR_PROMPT_STATUS
            }
            Self::DisplayNotify {
                timeout_seconds,
                text,
            } => {
                if session.uses_dynamic_general_ui() {
                    p = encode(
                        wire_id::DISPLAY_DYNAMIC_NOTIFY,
                        &WireDynamicNotifyHeader {
                            timeout_seconds: *timeout_seconds,
                        },
                    )?;
                    push_dynamic_text(
                        &mut p,
                        wire_id::DISPLAY_DYNAMIC_NOTIFY,
                        "notification",
                        text,
                        96,
                    )?;
                    pad_dynamic_payload(&mut p);
                    wire_id::DISPLAY_DYNAMIC_NOTIFY
                } else {
                    p = encode(
                        wire_id::DISPLAY_NOTIFY,
                        &WireNotify {
                            timeout_seconds: *timeout_seconds,
                            text: WireFixedText::new(
                                wire_id::DISPLAY_NOTIFY,
                                "notification",
                                text,
                            )?,
                        },
                    )?;
                    wire_id::DISPLAY_NOTIFY
                }
            }
            Self::ClearNotify => wire_id::CLEAR_NOTIFY,
            Self::DisplayPriorityNotify {
                timeout_seconds,
                priority,
                text,
            } => {
                if session.uses_dynamic_general_ui() {
                    p = encode(
                        wire_id::DISPLAY_DYNAMIC_PRIORITY_NOTIFY,
                        &WireDynamicPriorityNotifyHeader {
                            timeout_seconds: *timeout_seconds,
                            priority: priority.wire_value(),
                        },
                    )?;
                    push_dynamic_text(
                        &mut p,
                        wire_id::DISPLAY_DYNAMIC_PRIORITY_NOTIFY,
                        "notification",
                        text,
                        96,
                    )?;
                    pad_dynamic_payload(&mut p);
                    wire_id::DISPLAY_DYNAMIC_PRIORITY_NOTIFY
                } else {
                    p = encode(
                        wire_id::DISPLAY_PRIORITY_NOTIFY,
                        &WirePriorityNotify {
                            timeout_seconds: *timeout_seconds,
                            priority: priority.wire_value(),
                            text: WireFixedText::new(
                                wire_id::DISPLAY_PRIORITY_NOTIFY,
                                "notification",
                                text,
                            )?,
                        },
                    )?;
                    wire_id::DISPLAY_PRIORITY_NOTIFY
                }
            }
            Self::ClearPriorityNotify { priority } => {
                p = encode(
                    wire_id::CLEAR_PRIORITY_NOTIFY,
                    &WireOneWord {
                        value: priority.wire_value(),
                    },
                )?;
                wire_id::CLEAR_PRIORITY_NOTIFY
            }
            Self::NotifyDtmfTone(message) | Self::SendDtmfTone(message) => {
                let message_id = if matches!(self, Self::NotifyDtmfTone(_)) {
                    wire_id::NOTIFY_DTMF_TONE
                } else {
                    wire_id::SEND_DTMF_TONE
                };
                p = encode(
                    message_id,
                    &WireDtmfToneControl {
                        tone: message.tone.wire_value(),
                        conference_id: message.conference_id.get(),
                        passthrough_party_id: message.passthrough_party_id,
                    },
                )?;
                message_id
            }
            Self::StartAnnouncement {
                announcements,
                end_of_ack,
                conference_id,
                matrix_conference_party_ids,
                hearing_conference_party_mask,
                play_mode,
            } => {
                if announcements.len() > 32 {
                    return Err(CodecError::CountTooLarge {
                        message_id: wire_id::START_ANNOUNCEMENT,
                        field: "announcements",
                        count: announcements.len(),
                        maximum: 32,
                    });
                }
                if matrix_conference_party_ids.len() > 16 {
                    return Err(CodecError::CountTooLarge {
                        message_id: wire_id::START_ANNOUNCEMENT,
                        field: "matrix conference party identifiers",
                        count: matrix_conference_party_ids.len(),
                        maximum: 16,
                    });
                }
                let mut wire_announcements = [WireAnnouncementEntry::default(); 32];
                for (wire, entry) in wire_announcements.iter_mut().zip(announcements) {
                    *wire = WireAnnouncementEntry {
                        locale: entry.locale,
                        country: entry.country,
                        tone: entry.tone.wire_value(),
                    };
                }
                let mut wire_party_ids = [0; 16];
                wire_party_ids[..matrix_conference_party_ids.len()]
                    .copy_from_slice(matrix_conference_party_ids);
                p = encode(
                    wire_id::START_ANNOUNCEMENT,
                    &WireStartAnnouncement {
                        announcements: wire_announcements,
                        end_of_ack: *end_of_ack,
                        conference_id: *conference_id,
                        matrix_conference_party_ids: wire_party_ids,
                        hearing_conference_party_mask: *hearing_conference_party_mask,
                        play_mode: *play_mode,
                    },
                )?;
                wire_id::START_ANNOUNCEMENT
            }
            Self::StopAnnouncement { conference_id } => {
                p = encode(
                    wire_id::STOP_ANNOUNCEMENT,
                    &WireOneWord {
                        value: *conference_id,
                    },
                )?;
                wire_id::STOP_ANNOUNCEMENT
            }
            Self::AnnouncementFinish {
                conference_id,
                play_status,
            } => {
                p = encode(
                    wire_id::ANNOUNCEMENT_FINISH,
                    &WireAnnouncementFinish {
                        conference_id: *conference_id,
                        play_status: *play_status,
                    },
                )?;
                wire_id::ANNOUNCEMENT_FINISH
            }
            Self::ClearConference {
                conference_id,
                service_number,
            } => {
                p = encode(
                    wire_id::CLEAR_CONFERENCE,
                    &WireCallParty {
                        call_reference: conference_id.get(),
                        passthrough_party_id: *service_number,
                    },
                )?;
                wire_id::CLEAR_CONFERENCE
            }
            Self::CreateConferenceRequest(request) => {
                p = encode(
                    wire_id::CREATE_CONFERENCE_REQ,
                    &WireCreateConferenceRequest {
                        conference_id: request.conference_id.get(),
                        reserved_participants: request.reserved_participants,
                        resource_type: request.resource_type.wire_value(),
                        application_id: request.application_id.get(),
                        application_conference_id: WireFixedText::new(
                            wire_id::CREATE_CONFERENCE_REQ,
                            "application conference ID",
                            &request.application_conference_id,
                        )?,
                        application_data: WireFixedText::new(
                            wire_id::CREATE_CONFERENCE_REQ,
                            "application data",
                            &request.application_data,
                        )?,
                        data_length: validate_conference_data_for_encode(
                            wire_id::CREATE_CONFERENCE_REQ,
                            &request.passthrough_data,
                        )?,
                        passthrough_data: request.passthrough_data.clone(),
                    },
                )?;
                wire_id::CREATE_CONFERENCE_REQ
            }
            Self::DeleteConferenceRequest { conference_id } => {
                p = encode(
                    wire_id::DELETE_CONFERENCE_REQ,
                    &WireOneWord {
                        value: conference_id.get(),
                    },
                )?;
                wire_id::DELETE_CONFERENCE_REQ
            }
            Self::ModifyConferenceRequest(request) => {
                p = encode(
                    wire_id::MODIFY_CONFERENCE_REQ,
                    &WireModifyConferenceRequest {
                        conference_id: request.conference_id.get(),
                        reserved_participants: request.reserved_participants,
                        application_id: request.application_id.get(),
                        application_conference_id: WireFixedText::new(
                            wire_id::MODIFY_CONFERENCE_REQ,
                            "application conference ID",
                            &request.application_conference_id,
                        )?,
                        application_data: WireFixedText::new(
                            wire_id::MODIFY_CONFERENCE_REQ,
                            "application data",
                            &request.application_data,
                        )?,
                        data_length: validate_conference_data_for_encode(
                            wire_id::MODIFY_CONFERENCE_REQ,
                            &request.passthrough_data,
                        )?,
                        passthrough_data: request.passthrough_data.clone(),
                    },
                )?;
                wire_id::MODIFY_CONFERENCE_REQ
            }
            Self::AuditConferenceRequest => wire_id::AUDIT_CONFERENCE_REQ,
            Self::AddParticipantRequest(request) => {
                p = encode(
                    wire_id::ADD_PARTICIPANT_REQ,
                    &encode_participant_request(
                        wire_id::ADD_PARTICIPANT_REQ,
                        request.conference_id,
                        &request.participant,
                    )?,
                )?;
                wire_id::ADD_PARTICIPANT_REQ
            }
            Self::DropParticipantRequest {
                conference_id,
                call_reference,
            } => {
                p = encode(
                    wire_id::DROP_PARTICIPANT_REQ,
                    &WireCallParty {
                        call_reference: conference_id.get(),
                        passthrough_party_id: call_reference.get(),
                    },
                )?;
                wire_id::DROP_PARTICIPANT_REQ
            }
            Self::AuditParticipantRequest { conference_id } => {
                p = encode(
                    wire_id::AUDIT_PARTICIPANT_REQ,
                    &WireOneWord {
                        value: conference_id.get(),
                    },
                )?;
                wire_id::AUDIT_PARTICIPANT_REQ
            }
            Self::ChangeParticipantRequest(request) => {
                p = encode(
                    wire_id::CHANGE_PARTICIPANT_REQ,
                    &encode_participant_request(
                        wire_id::CHANGE_PARTICIPANT_REQ,
                        request.conference_id,
                        &request.participant,
                    )?,
                )?;
                wire_id::CHANGE_PARTICIPANT_REQ
            }
            Self::StopMultimediaTransmission(message)
            | Self::CloseMultimediaReceiveChannel(message) => {
                let message_id = if matches!(self, Self::StopMultimediaTransmission(_)) {
                    wire_id::STOP_MULTIMEDIA_TRANSMISSION
                } else {
                    wire_id::CLOSE_MULTIMEDIA_RECEIVE_CHANNEL
                };
                p = encode(
                    message_id,
                    &WireMultimediaStreamControl {
                        conference_id: message.conference_id.get(),
                        passthrough_party_id: message.passthrough_party_id.get(),
                        call_reference: message.call_reference.get(),
                        port_handling_flag: message.port_handling_flag,
                    },
                )?;
                message_id
            }
            Self::FlowControlCommand(message) | Self::FlowControlNotify(message) => {
                let message_id = if matches!(self, Self::FlowControlCommand(_)) {
                    wire_id::FLOW_CONTROL_COMMAND
                } else {
                    wire_id::FLOW_CONTROL_NOTIFY
                };
                p = encode(
                    message_id,
                    &WireVideoFlowControl {
                        conference_id: message.conference_id.get(),
                        passthrough_party_id: message.passthrough_party_id.get(),
                        call_reference: message.call_reference.get(),
                        maximum_bit_rate: message.maximum_bit_rate,
                    },
                )?;
                message_id
            }
            Self::VideoDisplayCommand {
                conference_id,
                call_reference,
                layout_id,
            } => {
                p = encode(
                    wire_id::VIDEO_DISPLAY_COMMAND,
                    &WireVideoDisplayCommand {
                        conference_id: conference_id.get(),
                        call_reference: call_reference.get(),
                        layout_id: *layout_id,
                    },
                )?;
                wire_id::VIDEO_DISPLAY_COMMAND
            }
            Self::ActivateCallPlane { line_instance } => {
                p = encode(
                    wire_id::ACTIVATE_CALL_PLANE,
                    &WireOneWord {
                        value: *line_instance,
                    },
                )?;
                wire_id::ACTIVATE_CALL_PLANE
            }
            Self::DeactivateCallPlane => wire_id::DEACTIVATE_CALL_PLANE,
            Self::BackspaceResponse {
                line_instance,
                call_reference,
            } => {
                p = encode(
                    wire_id::BACKSPACE_RESPONSE,
                    &WireLineCall {
                        line_instance: *line_instance,
                        call_reference: *call_reference,
                    },
                )?;
                wire_id::BACKSPACE_RESPONSE
            }
            Self::RegisterTokenAck => wire_id::REGISTER_TOKEN_ACK,
            Self::RegisterTokenReject { backoff_seconds } => {
                p = encode(
                    wire_id::REGISTER_TOKEN_REJECT,
                    &WireOneWord {
                        value: *backoff_seconds,
                    },
                )?;
                wire_id::REGISTER_TOKEN_REJECT
            }
            Self::SpcpRegisterTokenAck { features } => {
                p = encode(
                    wire_id::SPCP_REGISTER_TOKEN_ACK,
                    &WireOneWord { value: *features },
                )?;
                wire_id::SPCP_REGISTER_TOKEN_ACK
            }
            Self::SpcpRegisterTokenReject { backoff_seconds } => {
                p = encode(
                    wire_id::SPCP_REGISTER_TOKEN_REJECT,
                    &WireOneWord {
                        value: *backoff_seconds,
                    },
                )?;
                wire_id::SPCP_REGISTER_TOKEN_REJECT
            }
            Self::SetRinger {
                mode,
                duration,
                line_instance,
                call_reference,
            } => {
                p = encode(
                    wire_id::SET_RINGER,
                    &WireModeLineCall {
                        mode: mode.wire_value(),
                        duration: duration.wire_value(),
                        line_instance: *line_instance,
                        call_reference: *call_reference,
                    },
                )?;
                wire_id::SET_RINGER
            }
            Self::SetLamp {
                stimulus,
                instance,
                mode,
            } => {
                p = encode(
                    wire_id::SET_LAMP,
                    &WireLampState {
                        stimulus: stimulus.wire_value(),
                        instance: *instance,
                        mode: mode.wire_value(),
                    },
                )?;
                wire_id::SET_LAMP
            }
            Self::SetHookFlashDetect => wire_id::SET_HOOK_FLASH_DETECT,
            Self::StartTone {
                tone,
                direction,
                line_instance,
                call_reference,
            } => {
                p = encode(
                    wire_id::START_TONE,
                    &WireToneLineCall {
                        tone: tone.wire_value(),
                        direction: direction.wire_value(),
                        line_instance: *line_instance,
                        call_reference: *call_reference,
                    },
                )?;
                wire_id::START_TONE
            }
            Self::StopTone {
                line_instance,
                call_reference,
            } => {
                p = match protocol.wire() {
                    12.. => encode(
                        wire_id::STOP_TONE,
                        &WireStopToneV12 {
                            line_instance: *line_instance,
                            call_reference: *call_reference,
                            tone: 0,
                        },
                    ),
                    _ => encode(
                        wire_id::STOP_TONE,
                        &WireLineCall {
                            line_instance: *line_instance,
                            call_reference: *call_reference,
                        },
                    ),
                }?;
                wire_id::STOP_TONE
            }
            Self::StartMulticastMediaReception(message) => {
                p = encode_start_multicast_reception(message, protocol)?;
                wire_id::START_MULTICAST_MEDIA_RECEPTION
            }
            Self::StartMulticastMediaTransmission(message) => {
                p = encode_start_multicast_transmission(message, protocol)?;
                wire_id::START_MULTICAST_MEDIA_TRANSMISSION
            }
            Self::StopMulticastMediaReception {
                conference_id,
                passthrough_party_id,
                call_reference,
            }
            | Self::StopMulticastMediaTransmission {
                conference_id,
                passthrough_party_id,
                call_reference,
            } => {
                let message_id = if matches!(self, Self::StopMulticastMediaReception { .. }) {
                    wire_id::STOP_MULTICAST_MEDIA_RECEPTION
                } else {
                    wire_id::STOP_MULTICAST_MEDIA_TRANSMISSION
                };
                p = encode(
                    message_id,
                    &WireStopMulticast {
                        conference_id: conference_id.get(),
                        passthrough_party_id: passthrough_party_id.get(),
                        call_reference: call_reference.get(),
                    },
                )?;
                message_id
            }
            Self::OpenReceiveChannel {
                call_reference,
                passthrough_party_id,
                packet_ms,
                codec,
                echo_cancellation,
                telephone_event_payload,
                source_address,
                source_port,
                encryption,
                wire,
            } => {
                p = encode_open_receive(
                    *call_reference,
                    *passthrough_party_id,
                    OpenReceiveParameters {
                        packet_ms: *packet_ms,
                        codec: *codec,
                        echo_cancellation: *echo_cancellation,
                        telephone_event_payload: *telephone_event_payload,
                        source_address: *source_address,
                        source_port: *source_port,
                    },
                    encryption.as_ref(),
                    wire.as_ref(),
                    protocol,
                )?;
                wire_id::OPEN_RECEIVE_CHANNEL
            }
            Self::CloseReceiveChannel(control) => {
                p = encode(
                    wire_id::CLOSE_RECEIVE_CHANNEL,
                    &WireAudioStreamControl {
                        conference_id: control.conference_id.get(),
                        passthrough_party_id: control.passthrough_party_id.get(),
                        call_reference: control.call_reference.get(),
                        port_handling_flag: control.port_handling_flag,
                    },
                )?;
                wire_id::CLOSE_RECEIVE_CHANNEL
            }
            Self::ConnectionStatisticsRequest {
                directory_number,
                call_reference,
                processing,
            } => {
                p = match protocol.wire() {
                    19.. => encode_connection_statistics_request::<25, 3>(
                        directory_number,
                        *call_reference,
                        *processing,
                    ),
                    _ => encode_connection_statistics_request::<24, 0>(
                        directory_number,
                        *call_reference,
                        *processing,
                    ),
                }?;
                wire_id::CONNECTION_STATISTICS_REQ
            }
            Self::StartMediaTransmission {
                call_reference,
                passthrough_party_id,
                endpoint,
                silence_suppression,
                traffic_class,
                encryption,
                wire,
            } => {
                p = encode_start_media(
                    *call_reference,
                    *passthrough_party_id,
                    StartMediaParameters {
                        endpoint: *endpoint,
                        silence_suppression: *silence_suppression,
                        traffic_class: *traffic_class,
                    },
                    encryption.as_ref(),
                    wire.as_ref(),
                    protocol,
                )?;
                wire_id::START_MEDIA_TRANSMISSION
            }
            Self::StopMediaTransmission(control) => {
                p = encode(
                    wire_id::STOP_MEDIA_TRANSMISSION,
                    &WireAudioStreamControl {
                        conference_id: control.conference_id.get(),
                        passthrough_party_id: control.passthrough_party_id.get(),
                        call_reference: control.call_reference.get(),
                        port_handling_flag: control.port_handling_flag,
                    },
                )?;
                wire_id::STOP_MEDIA_TRANSMISSION
            }
            Self::StartMediaReception => wire_id::START_MEDIA_RECEPTION,
            Self::StopMediaReception {
                conference_id,
                passthrough_party_id,
            } => {
                p = encode(
                    wire_id::STOP_MEDIA_RECEPTION,
                    &WireStopMediaReception {
                        conference_id: conference_id.get(),
                        passthrough_party_id: passthrough_party_id.get(),
                    },
                )?;
                wire_id::STOP_MEDIA_RECEPTION
            }
            Self::SetSpeakerMode(mode) => {
                p = encode(
                    wire_id::SET_SPEAKER_MODE,
                    &WireOneWord {
                        value: mode.wire_value(),
                    },
                )?;
                wire_id::SET_SPEAKER_MODE
            }
            Self::SetMicrophoneMode(mode) => {
                p = encode(
                    wire_id::SET_MICROPHONE_MODE,
                    &WireOneWord {
                        value: mode.wire_value(),
                    },
                )?;
                wire_id::SET_MICROPHONE_MODE
            }
            Self::Reset(reset) => {
                p = encode(
                    wire_id::RESET,
                    &WireOneWord {
                        value: reset.wire_value(),
                    },
                )?;
                wire_id::RESET
            }
            Self::DisplayText { text } => {
                p = encode(
                    wire_id::DISPLAY_TEXT,
                    &WireFixedText::<32>::new(wire_id::DISPLAY_TEXT, "display text", text)?,
                )?;
                wire_id::DISPLAY_TEXT
            }
            Self::ClearDisplay => wire_id::CLEAR_DISPLAY,
            Self::ForwardStatus {
                line_instance,
                forward_all,
                forward_busy,
                forward_no_answer,
            } => {
                p = match protocol.wire() {
                    19.. => encode_forward_status::<25, 3>(
                        *line_instance,
                        forward_all.as_deref(),
                        forward_busy.as_deref(),
                        forward_no_answer.as_deref(),
                    ),
                    _ => encode_forward_status::<24, 0>(
                        *line_instance,
                        forward_all.as_deref(),
                        forward_busy.as_deref(),
                        forward_no_answer.as_deref(),
                    ),
                }?;
                wire_id::FORWARD_STAT
            }
            Self::SpeedDialStatus {
                instance,
                number,
                display_name,
            } => {
                if session.uses_dynamic_speed_dial_status() {
                    p = encode_dynamic_speed_dial_status(
                        *instance,
                        number,
                        display_name,
                        legacy_code_page,
                    )?;
                    wire_id::SPEED_DIAL_STAT_DYNAMIC
                } else {
                    p = encode(
                        wire_id::SPEED_DIAL_STAT,
                        &WireSpeedDialStatus {
                            instance: *instance,
                            number: WireFixedText::new(wire_id::SPEED_DIAL_STAT, "number", number)?,
                            display_name: WireFixedText::new_station(
                                wire_id::SPEED_DIAL_STAT,
                                "display name",
                                display_name,
                                legacy_code_page,
                            )?,
                        },
                    )?;
                    wire_id::SPEED_DIAL_STAT
                }
            }
            Self::DialedNumber {
                number,
                line_instance,
                call_reference,
            } => {
                p = match protocol.wire() {
                    19.. => encode_dialed_number::<25, 3>(number, *line_instance, *call_reference),
                    _ => encode_dialed_number::<24, 0>(number, *line_instance, *call_reference),
                }?;
                wire_id::DIALED_NUMBER
            }
            Self::StartMediaFailureDetection(detection) => {
                p = encode(
                    wire_id::START_MEDIA_FAILURE_DETECTION,
                    &WireMediaFailureDetection {
                        conference_id: detection.conference_id.get(),
                        passthrough_party_id: detection.passthrough_party_id,
                        packet_millis: detection.packet_millis,
                        codec: detection.codec.wire_value(),
                        echo_cancellation: detection.echo_cancellation.wire_value(),
                        codec_qualifier: detection.codec_qualifier,
                        call_reference: detection.call_reference.get(),
                    },
                )?;
                wire_id::START_MEDIA_FAILURE_DETECTION
            }
            Self::OpenMultimediaChannel(message) => {
                p = encode_open_multimedia(message, protocol)?;
                wire_id::OPEN_MULTIMEDIA_CHANNEL
            }
            Self::StartMultimediaTransmission(message) => {
                p = encode_start_multimedia(message, protocol)?;
                wire_id::START_MULTIMEDIA_TRANSMISSION
            }
            Self::MiscellaneousCommand(message) => {
                p = encode_miscellaneous_command(message)?;
                wire_id::MISCELLANEOUS_COMMAND
            }
            Self::UserToDeviceData(data) => {
                p = encode_user_data(data, wire_id::USER_TO_DEVICE_DATA)?;
                wire_id::USER_TO_DEVICE_DATA
            }
            Self::UserToDeviceDataV1(data) => {
                p = encode_user_data_v1(data, wire_id::USER_TO_DEVICE_DATA_V1)?;
                wire_id::USER_TO_DEVICE_DATA_V1
            }
            Self::SubscribeDtmfPayloadRequest(request) => {
                p = encode(
                    wire_id::SUBSCRIBE_DTMF_PAYLOAD_REQ,
                    &dtmf_payload_request_to_wire(*request),
                )?;
                wire_id::SUBSCRIBE_DTMF_PAYLOAD_REQ
            }
            Self::SubscribeDtmfPayloadError(identity) => {
                p = encode(
                    wire_id::SUBSCRIBE_DTMF_PAYLOAD_ERR,
                    &dtmf_payload_identity_to_wire(*identity),
                )?;
                wire_id::SUBSCRIBE_DTMF_PAYLOAD_ERR
            }
            Self::UnsubscribeDtmfPayloadRequest(request) => {
                p = encode(
                    wire_id::UNSUBSCRIBE_DTMF_PAYLOAD_REQ,
                    &dtmf_payload_request_to_wire(*request),
                )?;
                wire_id::UNSUBSCRIBE_DTMF_PAYLOAD_REQ
            }
            Self::UnsubscribeDtmfPayloadError(identity) => {
                p = encode(
                    wire_id::UNSUBSCRIBE_DTMF_PAYLOAD_ERR,
                    &dtmf_payload_identity_to_wire(*identity),
                )?;
                wire_id::UNSUBSCRIBE_DTMF_PAYLOAD_ERR
            }
            Self::FeatureStatus {
                instance,
                button_type,
                label,
                state,
            } => {
                if session.uses_dynamic_feature_status() {
                    p = encode(
                        wire_id::FEATURE_STAT_DYNAMIC,
                        &WireFeatureStatusDynamic {
                            instance: *instance,
                            button_type: button_type.wire_value(),
                            state: *state,
                            label: WireFixedText::new_station(
                                wire_id::FEATURE_STAT_DYNAMIC,
                                "feature label",
                                label,
                                legacy_code_page,
                            )?,
                            padding: [0; 3],
                        },
                    )?;
                    wire_id::FEATURE_STAT_DYNAMIC
                } else {
                    p = encode(
                        wire_id::FEATURE_STAT,
                        &WireFeatureStatus {
                            instance: *instance,
                            button_type: button_type.wire_value(),
                            label: WireFixedText::new_station(
                                wire_id::FEATURE_STAT,
                                "feature label",
                                label,
                                legacy_code_page,
                            )?,
                            state: *state,
                        },
                    )?;
                    wire_id::FEATURE_STAT
                }
            }
            Self::ServiceUrlStatus {
                index,
                url,
                label,
                extension_text,
            } => {
                match (protocol.wire(), extension_text.is_empty()) {
                    (0..=18, false) => Err(CodecError::InvalidValue {
                        message_id: wire_id::SERVICE_URL_STAT_DYNAMIC,
                        field: "service URL extension for this protocol version",
                        value: extension_text.len() as u64,
                    }),
                    _ => Ok(()),
                }?;
                if session.uses_dynamic_general_ui() {
                    p = encode_dynamic_service_url_status(
                        *index,
                        url,
                        label,
                        extension_text,
                        protocol,
                        legacy_code_page,
                    )?;
                    wire_id::SERVICE_URL_STAT_DYNAMIC
                } else {
                    p = encode(
                        wire_id::SERVICE_URL_STAT,
                        &WireServiceUrlStatus {
                            index: *index,
                            url: WireFixedText::new(wire_id::SERVICE_URL_STAT, "service URL", url)?,
                            label: WireFixedText::new_station(
                                wire_id::SERVICE_URL_STAT,
                                "service label",
                                label,
                                legacy_code_page,
                            )?,
                        },
                    )?;
                    wire_id::SERVICE_URL_STAT
                }
            }
            Self::CallSelectStatus {
                status,
                call_reference,
                line_instance,
            } => {
                p = encode(
                    wire_id::CALL_SELECT_STAT,
                    &WireCallSelectStatus {
                        status: *status,
                        call_reference: *call_reference,
                        line_instance: *line_instance,
                    },
                )?;
                wire_id::CALL_SELECT_STAT
            }
            Self::PortRequest(request) => {
                let base = WirePortRequest {
                    conference_id: request.conference_id.get(),
                    call_reference: request.call_reference.get(),
                    passthrough_party_id: request.passthrough_party_id.get(),
                    transport: request.transport.wire_value(),
                };
                p = match protocol.wire() {
                    20.. => encode(
                        wire_id::PORT_REQUEST,
                        &WirePortRequestV20 {
                            base,
                            address_type: request
                                .address_type
                                .ok_or(CodecError::InvalidValue {
                                    message_id: wire_id::PORT_REQUEST,
                                    field: "address type required from protocol 20",
                                    value: 0,
                                })?
                                .wire_value(),
                            media_type: request
                                .media_type
                                .ok_or(CodecError::InvalidValue {
                                    message_id: wire_id::PORT_REQUEST,
                                    field: "media type required from protocol 20",
                                    value: 0,
                                })?
                                .wire_value(),
                        },
                    ),
                    _ => encode(wire_id::PORT_REQUEST, &base),
                }?;
                wire_id::PORT_REQUEST
            }
            Self::PortClose(close) => {
                let base = WirePortClose {
                    conference_id: close.conference_id.get(),
                    call_reference: close.call_reference.get(),
                    passthrough_party_id: close.passthrough_party_id.get(),
                };
                p = match protocol.wire() {
                    20.. => encode(
                        wire_id::PORT_CLOSE,
                        &WirePortCloseV20 {
                            base,
                            media_type: close
                                .media_type
                                .ok_or(CodecError::InvalidValue {
                                    message_id: wire_id::PORT_CLOSE,
                                    field: "media type required from protocol 20",
                                    value: 0,
                                })?
                                .wire_value(),
                        },
                    ),
                    _ => encode(wire_id::PORT_CLOSE, &base),
                }?;
                wire_id::PORT_CLOSE
            }
            Self::SubscriptionStatus {
                transaction_id,
                feature_id,
                timer_seconds,
                cause,
            } => {
                p = encode(
                    wire_id::SUBSCRIPTION_STAT,
                    &WireSubscriptionStatus {
                        transaction_id: *transaction_id,
                        feature_id: *feature_id,
                        timer_seconds: *timer_seconds,
                        cause: cause.wire_value(),
                    },
                )?;
                wire_id::SUBSCRIPTION_STAT
            }
            Self::Notification {
                transaction_id,
                feature_id,
                status,
                text,
            } => {
                p = encode(
                    wire_id::NOTIFICATION,
                    &WireNotification {
                        transaction_id: *transaction_id,
                        feature_id: *feature_id,
                        status: status.wire_value(),
                        text: WireFixedText::new(wire_id::NOTIFICATION, "notification", text)?,
                    },
                )?;
                wire_id::NOTIFICATION
            }
            Self::CallHistoryDisposition {
                disposition,
                line_instance,
                call_reference,
            } => {
                p = encode(
                    wire_id::CALL_HISTORY_DISPOSITION,
                    &WireCallHistoryDisposition {
                        disposition: disposition.wire_value(),
                        line_instance: *line_instance,
                        call_reference: *call_reference,
                    },
                )?;
                wire_id::CALL_HISTORY_DISPOSITION
            }
            Self::CallCountResponse(response) => {
                if response.line_data.len() > CALL_COUNT_RESPONSE_MAX_LINE_ENTRIES {
                    return Err(CodecError::CountTooLarge {
                        message_id: wire_id::CALL_COUNT_RES,
                        field: "call-count line data",
                        count: response.line_data.len(),
                        maximum: CALL_COUNT_RESPONSE_MAX_LINE_ENTRIES,
                    });
                }
                let mut line_data =
                    [WireCallCountLineData::default(); CALL_COUNT_RESPONSE_MAX_LINE_ENTRIES];
                for (wire, entry) in line_data.iter_mut().zip(&response.line_data) {
                    *wire = WireCallCountLineData {
                        max_calls: entry.max_calls,
                        busy_trigger: entry.busy_trigger,
                    };
                }
                p = encode(
                    wire_id::CALL_COUNT_RES,
                    &WireCallCountResponse {
                        total_configured_lines: response.total_configured_lines,
                        starting_line_instance: response.starting_line_instance,
                        line_data_entries: wire_count(
                            wire_id::CALL_COUNT_RES,
                            "call-count line data",
                            response.line_data.len(),
                        )?,
                        line_data,
                    },
                )?;
                wire_id::CALL_COUNT_RES
            }
            Self::RecordingStatus {
                call_reference,
                active,
            } => {
                p = encode(
                    wire_id::RECORDING_STATUS,
                    &WireRecordingStatus {
                        call_reference: *call_reference,
                        active: u32::from(*active),
                    },
                )?;
                wire_id::RECORDING_STATUS
            }
            Self::KnownOpaque(message) => {
                ensure_preserve_only(message.id)?;
                return Ok((
                    message.id.wire_value(),
                    message.payload.as_bytes().to_vec(),
                    message.protocol_version,
                ));
            }
            Self::Unknown(message) => {
                return Ok((
                    message.message_id,
                    message.payload.clone(),
                    message.protocol_version,
                ));
            }
        };
        pad_typed_payload(id, &mut p);
        Ok((id, p, protocol.wire()))
    }
}

fn reject_non_station_route(
    message_id: u32,
    expected_route: MessageRoute,
    expected: &'static str,
) -> Result<(), CodecError> {
    if let Some(actual) = MessageId::from(message_id).route()
        && actual != expected_route
    {
        return Err(CodecError::UnexpectedRoute {
            message_id,
            actual,
            expected,
        });
    }
    Ok(())
}

impl ControlMessage {
    /// Decode a frame whose catalog route is between call-control or service
    /// roles. Station messages fail closed instead of being interpreted by a
    /// structurally similar conference or QoS layout.
    pub fn decode(frame: Frame, protocol: ProtocolVersion) -> Result<Self, CodecError> {
        let message_id = MessageId::from(frame.message_id);
        let route = message_id.route().ok_or(CodecError::InvalidValue {
            message_id: frame.message_id,
            field: "known control message identifier",
            value: u64::from(frame.message_id),
        })?;
        if matches!(
            route,
            MessageRoute::StationToControl | MessageRoute::ControlToStation
        ) {
            return Err(CodecError::UnexpectedRoute {
                message_id: frame.message_id,
                actual: route,
                expected: "control/service-node or intra-control route",
            });
        }

        let p = &frame.payload;
        match frame.message_id {
            wire_id::START_SESSION_TRANSMISSION | wire_id::STOP_SESSION_TRANSMISSION => {
                let message = decode_session_transmission(p, protocol, frame.message_id)?;
                if frame.message_id == wire_id::START_SESSION_TRANSMISSION {
                    Ok(Self::StartSessionTransmission(message))
                } else {
                    Ok(Self::StopSessionTransmission(message))
                }
            }
            wire_id::QOS_RESERVATION_NOTIFY => {
                let value: WireQosReservationNotify = decode(frame.message_id, p)?;
                Ok(Self::QosReservationNotify {
                    flow: qos_flow_from_wire(value.flow, frame.message_id)?,
                    direction: QosDirection::from(value.direction),
                })
            }
            wire_id::QOS_ERROR_NOTIFY => {
                let value: WireQosErrorNotify = decode(frame.message_id, p)?;
                Ok(Self::QosErrorNotify {
                    flow: qos_flow_from_wire(value.flow, frame.message_id)?,
                    direction: QosDirection::from(value.direction),
                    error_code: QosErrorCode::from(value.error_code),
                    failure_node: Ipv4Addr::from(value.failure_node),
                    rsvp_error_code: RsvpErrorCode::from(value.rsvp_error_code),
                    rsvp_error_subcode: value.rsvp_error_subcode,
                    rsvp_error_flags: value.rsvp_error_flags,
                })
            }
            wire_id::QOS_LISTEN => {
                let value: WireQosListen = decode(frame.message_id, p)?;
                Ok(Self::QosListen {
                    flow: qos_flow_from_wire(value.flow, frame.message_id)?,
                    reservation_style: QosReservationStyle::from(value.reservation_style),
                    maximum_retries: value.maximum_retries,
                    retry_timer: value.retry_timer,
                    confirmation_required: decode_bool_word(
                        value.confirmation_required,
                        frame.message_id,
                        "QoS confirmation required",
                    )?,
                    preemption_priority: value.preemption_priority,
                    defending_priority: value.defending_priority,
                    traffic: qos_traffic(
                        value.compression_type,
                        value.average_bit_rate,
                        value.burst_size,
                        value.peak_rate,
                    ),
                    application: qos_application_from_wire(value.application)?,
                })
            }
            wire_id::QOS_PATH => {
                let value: WireQosPath = decode(frame.message_id, p)?;
                Ok(Self::QosPath {
                    flow: qos_flow_from_wire(value.flow, frame.message_id)?,
                    reservation_style: QosReservationStyle::from(value.reservation_style),
                    maximum_retries: value.maximum_retries,
                    retry_timer: value.retry_timer,
                    preemption_priority: value.preemption_priority,
                    defending_priority: value.defending_priority,
                    traffic: qos_traffic(
                        value.compression_type,
                        value.average_bit_rate,
                        value.burst_size,
                        value.peak_rate,
                    ),
                    application: qos_application_from_wire(value.application)?,
                })
            }
            wire_id::QOS_TEARDOWN => {
                let value: WireQosReservationNotify = decode(frame.message_id, p)?;
                Ok(Self::QosTeardown {
                    flow: qos_flow_from_wire(value.flow, frame.message_id)?,
                    direction: QosDirection::from(value.direction),
                })
            }
            wire_id::UPDATE_DSCP => {
                let value: WireUpdateDscp = decode(frame.message_id, p)?;
                let dscp = u8::try_from(value.dscp).map_err(|_| CodecError::InvalidValue {
                    message_id: frame.message_id,
                    field: "DSCP",
                    value: u64::from(value.dscp),
                })?;
                if dscp > 63 {
                    return Err(CodecError::InvalidValue {
                        message_id: frame.message_id,
                        field: "DSCP",
                        value: u64::from(dscp),
                    });
                }
                Ok(Self::UpdateDscp {
                    flow: qos_flow_from_wire(value.flow, frame.message_id)?,
                    dscp,
                })
            }
            wire_id::QOS_MODIFY => {
                let value: WireQosModify = decode(frame.message_id, p)?;
                Ok(Self::QosModify {
                    flow: qos_flow_from_wire(value.flow, frame.message_id)?,
                    direction: QosDirection::from(value.direction),
                    traffic: qos_traffic(
                        value.compression_type,
                        value.average_bit_rate,
                        value.burst_size,
                        value.peak_rate,
                    ),
                    application: qos_application_from_wire(value.application)?,
                })
            }
            wire_id::MWI_NOTIFICATION => {
                let value: WireMessageWaitingNotification = decode(frame.message_id, p)?;
                validate_zero_payload(&value.alignment, frame.message_id, 2)?;
                Ok(Self::MessageWaitingNotification(
                    MessageWaitingNotification {
                        target_number: value.target_number.text()?,
                        control_number: value.control_number.text()?,
                        messages_waiting: decode_bool_word(
                            value.messages_waiting,
                            frame.message_id,
                            "messages waiting",
                        )?,
                        total_voicemail: MessageWaitingCounts {
                            new: value.total_voicemail_new,
                            old: value.total_voicemail_old,
                        },
                        priority_voicemail: MessageWaitingCounts {
                            new: value.priority_voicemail_new,
                            old: value.priority_voicemail_old,
                        },
                        total_fax: MessageWaitingCounts {
                            new: value.total_fax_new,
                            old: value.total_fax_old,
                        },
                        priority_fax: MessageWaitingCounts {
                            new: value.priority_fax_new,
                            old: value.priority_fax_old,
                        },
                    },
                ))
            }
            wire_id::MWI_RESPONSE => {
                let value: WireMessageWaitingResponse = decode(frame.message_id, p)?;
                validate_zero_payload(&value.alignment, frame.message_id, 3)?;
                Ok(Self::MessageWaitingResponse {
                    target_number: value.target_number.text()?,
                    result: MessageWaitingResult::from(value.result),
                })
            }
            wire_id::MEDIA_RESOURCE_NOTIFICATION
            | wire_id::PORT_RESPONSE
            | wire_id::CREATE_CONFERENCE_RES
            | wire_id::DELETE_CONFERENCE_RES
            | wire_id::MODIFY_CONFERENCE_RES
            | wire_id::ADD_PARTICIPANT_RES
            | wire_id::AUDIT_CONFERENCE_RES
            | wire_id::AUDIT_PARTICIPANT_RES => Self::from_client_message(
                ClientMessage::decode_using_protocol(frame, protocol.wire())?,
            ),
            wire_id::CLEAR_CONFERENCE
            | wire_id::START_ANNOUNCEMENT
            | wire_id::STOP_ANNOUNCEMENT
            | wire_id::ANNOUNCEMENT_FINISH
            | wire_id::CREATE_CONFERENCE_REQ
            | wire_id::DELETE_CONFERENCE_REQ
            | wire_id::MODIFY_CONFERENCE_REQ
            | wire_id::ADD_PARTICIPANT_REQ
            | wire_id::DROP_PARTICIPANT_REQ
            | wire_id::AUDIT_CONFERENCE_REQ
            | wire_id::AUDIT_PARTICIPANT_REQ
            | wire_id::CHANGE_PARTICIPANT_REQ => {
                Self::from_server_message(ServerMessage::decode_unchecked(frame, protocol)?)
            }
            _ => preserve_known_message(frame, message_id).map(Self::KnownOpaque),
        }
    }

    fn from_client_message(message: ClientMessage) -> Result<Self, CodecError> {
        Ok(match message {
            ClientMessage::MediaResourceNotification(value) => {
                Self::MediaResourceNotification(value)
            }
            ClientMessage::PortResponse(value) => Self::PortResponse(value),
            ClientMessage::CreateConferenceResponse(value) => Self::CreateConferenceResponse(value),
            ClientMessage::DeleteConferenceResponse {
                conference_id,
                result,
            } => Self::DeleteConferenceResponse {
                conference_id,
                result,
            },
            ClientMessage::ModifyConferenceResponse(value) => Self::ModifyConferenceResponse(value),
            ClientMessage::AddParticipantResponse(value) => Self::AddParticipantResponse(value),
            ClientMessage::AuditConferenceResponse(value) => Self::AuditConferenceResponse(value),
            ClientMessage::AuditParticipantResponse(value) => Self::AuditParticipantResponse(value),
            _ => {
                return Err(CodecError::InvalidValue {
                    message_id: 0,
                    field: "control message decoded through station codec",
                    value: 0,
                });
            }
        })
    }

    fn from_server_message(message: ServerMessage) -> Result<Self, CodecError> {
        Ok(match message {
            ServerMessage::ClearConference {
                conference_id,
                service_number,
            } => Self::ClearConference {
                conference_id,
                service_number,
            },
            ServerMessage::CreateConferenceRequest(value) => Self::CreateConferenceRequest(value),
            ServerMessage::DeleteConferenceRequest { conference_id } => {
                Self::DeleteConferenceRequest { conference_id }
            }
            ServerMessage::ModifyConferenceRequest(value) => Self::ModifyConferenceRequest(value),
            ServerMessage::AddParticipantRequest(value) => Self::AddParticipantRequest(value),
            ServerMessage::DropParticipantRequest {
                conference_id,
                call_reference,
            } => Self::DropParticipantRequest {
                conference_id,
                call_reference,
            },
            ServerMessage::AuditConferenceRequest => Self::AuditConferenceRequest,
            ServerMessage::AuditParticipantRequest { conference_id } => {
                Self::AuditParticipantRequest { conference_id }
            }
            ServerMessage::ChangeParticipantRequest(value) => Self::ChangeParticipantRequest(value),
            ServerMessage::StartAnnouncement {
                announcements,
                end_of_ack,
                conference_id,
                matrix_conference_party_ids,
                hearing_conference_party_mask,
                play_mode,
            } => Self::StartAnnouncement {
                announcements,
                end_of_ack: EndOfAnnouncementAck::from(end_of_ack),
                conference_id,
                matrix_conference_party_ids,
                hearing_conference_party_mask,
                play_mode: AnnouncementPlayMode::from(play_mode),
            },
            ServerMessage::StopAnnouncement { conference_id } => {
                Self::StopAnnouncement { conference_id }
            }
            ServerMessage::AnnouncementFinish {
                conference_id,
                play_status,
            } => Self::AnnouncementFinish {
                conference_id,
                play_status: AnnouncementPlayStatus::from(play_status),
            },
            _ => {
                return Err(CodecError::InvalidValue {
                    message_id: 0,
                    field: "control message decoded through station codec",
                    value: 0,
                });
            }
        })
    }

    /// Encodes a message routed between control and service roles.
    ///
    /// Station-routed variants are rejected rather than emitted through the
    /// control-message API.
    pub fn encode(&self, protocol: ProtocolVersion) -> Result<Vec<u8>, CodecError> {
        let (message_id, payload, protocol_version) = match self {
            Self::StartSessionTransmission(message) | Self::StopSessionTransmission(message) => {
                let message_id = if matches!(self, Self::StartSessionTransmission(_)) {
                    wire_id::START_SESSION_TRANSMISSION
                } else {
                    wire_id::STOP_SESSION_TRANSMISSION
                };
                (
                    message_id,
                    encode_session_transmission(*message, protocol, message_id)?,
                    protocol.wire(),
                )
            }
            Self::QosReservationNotify { flow, direction } => (
                wire_id::QOS_RESERVATION_NOTIFY,
                encode(
                    wire_id::QOS_RESERVATION_NOTIFY,
                    &WireQosReservationNotify {
                        flow: qos_flow_to_wire(*flow),
                        direction: direction.wire_value(),
                    },
                )?,
                protocol.wire(),
            ),
            Self::QosErrorNotify {
                flow,
                direction,
                error_code,
                failure_node,
                rsvp_error_code,
                rsvp_error_subcode,
                rsvp_error_flags,
            } => (
                wire_id::QOS_ERROR_NOTIFY,
                encode(
                    wire_id::QOS_ERROR_NOTIFY,
                    &WireQosErrorNotify {
                        flow: qos_flow_to_wire(*flow),
                        direction: direction.wire_value(),
                        error_code: error_code.wire_value(),
                        failure_node: u32::from(*failure_node),
                        rsvp_error_code: rsvp_error_code.wire_value(),
                        rsvp_error_subcode: *rsvp_error_subcode,
                        rsvp_error_flags: *rsvp_error_flags,
                    },
                )?,
                protocol.wire(),
            ),
            Self::QosListen {
                flow,
                reservation_style,
                maximum_retries,
                retry_timer,
                confirmation_required,
                preemption_priority,
                defending_priority,
                traffic,
                application,
            } => (
                wire_id::QOS_LISTEN,
                encode(
                    wire_id::QOS_LISTEN,
                    &WireQosListen {
                        flow: qos_flow_to_wire(*flow),
                        reservation_style: reservation_style.wire_value(),
                        maximum_retries: *maximum_retries,
                        retry_timer: *retry_timer,
                        confirmation_required: u32::from(*confirmation_required),
                        preemption_priority: *preemption_priority,
                        defending_priority: *defending_priority,
                        compression_type: traffic.codec.wire_value(),
                        average_bit_rate: traffic.average_bit_rate,
                        burst_size: traffic.burst_size,
                        peak_rate: traffic.peak_rate,
                        application: qos_application_to_wire(wire_id::QOS_LISTEN, application)?,
                    },
                )?,
                protocol.wire(),
            ),
            Self::QosPath {
                flow,
                reservation_style,
                maximum_retries,
                retry_timer,
                preemption_priority,
                defending_priority,
                traffic,
                application,
            } => (
                wire_id::QOS_PATH,
                encode(
                    wire_id::QOS_PATH,
                    &WireQosPath {
                        flow: qos_flow_to_wire(*flow),
                        reservation_style: reservation_style.wire_value(),
                        maximum_retries: *maximum_retries,
                        retry_timer: *retry_timer,
                        preemption_priority: *preemption_priority,
                        defending_priority: *defending_priority,
                        compression_type: traffic.codec.wire_value(),
                        average_bit_rate: traffic.average_bit_rate,
                        burst_size: traffic.burst_size,
                        peak_rate: traffic.peak_rate,
                        application: qos_application_to_wire(wire_id::QOS_PATH, application)?,
                    },
                )?,
                protocol.wire(),
            ),
            Self::QosTeardown { flow, direction } => (
                wire_id::QOS_TEARDOWN,
                encode(
                    wire_id::QOS_TEARDOWN,
                    &WireQosReservationNotify {
                        flow: qos_flow_to_wire(*flow),
                        direction: direction.wire_value(),
                    },
                )?,
                protocol.wire(),
            ),
            Self::UpdateDscp { flow, dscp } => {
                if *dscp > 63 {
                    return Err(CodecError::InvalidValue {
                        message_id: wire_id::UPDATE_DSCP,
                        field: "DSCP",
                        value: u64::from(*dscp),
                    });
                }
                (
                    wire_id::UPDATE_DSCP,
                    encode(
                        wire_id::UPDATE_DSCP,
                        &WireUpdateDscp {
                            flow: qos_flow_to_wire(*flow),
                            dscp: u32::from(*dscp),
                        },
                    )?,
                    protocol.wire(),
                )
            }
            Self::QosModify {
                flow,
                direction,
                traffic,
                application,
            } => (
                wire_id::QOS_MODIFY,
                encode(
                    wire_id::QOS_MODIFY,
                    &WireQosModify {
                        flow: qos_flow_to_wire(*flow),
                        direction: direction.wire_value(),
                        compression_type: traffic.codec.wire_value(),
                        average_bit_rate: traffic.average_bit_rate,
                        burst_size: traffic.burst_size,
                        peak_rate: traffic.peak_rate,
                        application: qos_application_to_wire(wire_id::QOS_MODIFY, application)?,
                    },
                )?,
                protocol.wire(),
            ),
            Self::MessageWaitingNotification(value) => (
                wire_id::MWI_NOTIFICATION,
                encode(
                    wire_id::MWI_NOTIFICATION,
                    &WireMessageWaitingNotification {
                        target_number: WireFixedText::new(
                            wire_id::MWI_NOTIFICATION,
                            "MWI target number",
                            &value.target_number,
                        )?,
                        control_number: WireFixedText::new(
                            wire_id::MWI_NOTIFICATION,
                            "MWI control number",
                            &value.control_number,
                        )?,
                        alignment: [0; 2],
                        messages_waiting: u32::from(value.messages_waiting),
                        total_voicemail_new: value.total_voicemail.new,
                        total_voicemail_old: value.total_voicemail.old,
                        priority_voicemail_new: value.priority_voicemail.new,
                        priority_voicemail_old: value.priority_voicemail.old,
                        total_fax_new: value.total_fax.new,
                        total_fax_old: value.total_fax.old,
                        priority_fax_new: value.priority_fax.new,
                        priority_fax_old: value.priority_fax.old,
                    },
                )?,
                protocol.wire(),
            ),
            Self::MessageWaitingResponse {
                target_number,
                result,
            } => (
                wire_id::MWI_RESPONSE,
                encode(
                    wire_id::MWI_RESPONSE,
                    &WireMessageWaitingResponse {
                        target_number: WireFixedText::new(
                            wire_id::MWI_RESPONSE,
                            "MWI target number",
                            target_number,
                        )?,
                        alignment: [0; 3],
                        result: result.wire_value(),
                    },
                )?,
                protocol.wire(),
            ),
            Self::KnownOpaque(message) => {
                ensure_preserve_only(message.id)?;
                return Frame::new(
                    message.protocol_version,
                    message.id.wire_value(),
                    message.payload.as_bytes().to_vec(),
                )
                .encode();
            }
            other => return other.encode_via_existing(protocol),
        };
        Frame::new(protocol_version, message_id, payload).encode()
    }

    fn encode_via_existing(&self, protocol: ProtocolVersion) -> Result<Vec<u8>, CodecError> {
        match self {
            Self::MediaResourceNotification(value) => {
                ClientMessage::MediaResourceNotification(value.clone()).encode_unchecked(protocol)
            }
            Self::PortResponse(value) => {
                ClientMessage::PortResponse(value.clone()).encode_unchecked(protocol)
            }
            Self::CreateConferenceResponse(value) => {
                ClientMessage::CreateConferenceResponse(value.clone()).encode_unchecked(protocol)
            }
            Self::DeleteConferenceResponse {
                conference_id,
                result,
            } => ClientMessage::DeleteConferenceResponse {
                conference_id: *conference_id,
                result: *result,
            }
            .encode_unchecked(protocol),
            Self::ModifyConferenceResponse(value) => {
                ClientMessage::ModifyConferenceResponse(value.clone()).encode_unchecked(protocol)
            }
            Self::AddParticipantResponse(value) => {
                ClientMessage::AddParticipantResponse(value.clone()).encode_unchecked(protocol)
            }
            Self::AuditConferenceResponse(value) => {
                ClientMessage::AuditConferenceResponse(value.clone()).encode_unchecked(protocol)
            }
            Self::AuditParticipantResponse(value) => {
                ClientMessage::AuditParticipantResponse(value.clone()).encode_unchecked(protocol)
            }
            Self::ClearConference {
                conference_id,
                service_number,
            } => ServerMessage::ClearConference {
                conference_id: *conference_id,
                service_number: *service_number,
            }
            .encode_unchecked(protocol),
            Self::CreateConferenceRequest(value) => {
                ServerMessage::CreateConferenceRequest(value.clone()).encode_unchecked(protocol)
            }
            Self::DeleteConferenceRequest { conference_id } => {
                ServerMessage::DeleteConferenceRequest {
                    conference_id: *conference_id,
                }
                .encode_unchecked(protocol)
            }
            Self::ModifyConferenceRequest(value) => {
                ServerMessage::ModifyConferenceRequest(value.clone()).encode_unchecked(protocol)
            }
            Self::AddParticipantRequest(value) => {
                ServerMessage::AddParticipantRequest(value.clone()).encode_unchecked(protocol)
            }
            Self::DropParticipantRequest {
                conference_id,
                call_reference,
            } => ServerMessage::DropParticipantRequest {
                conference_id: *conference_id,
                call_reference: *call_reference,
            }
            .encode_unchecked(protocol),
            Self::AuditConferenceRequest => {
                ServerMessage::AuditConferenceRequest.encode_unchecked(protocol)
            }
            Self::AuditParticipantRequest { conference_id } => {
                ServerMessage::AuditParticipantRequest {
                    conference_id: *conference_id,
                }
                .encode_unchecked(protocol)
            }
            Self::ChangeParticipantRequest(value) => {
                ServerMessage::ChangeParticipantRequest(value.clone()).encode_unchecked(protocol)
            }
            Self::StartAnnouncement {
                announcements,
                end_of_ack,
                conference_id,
                matrix_conference_party_ids,
                hearing_conference_party_mask,
                play_mode,
            } => ServerMessage::StartAnnouncement {
                announcements: announcements.clone(),
                end_of_ack: end_of_ack.wire_value(),
                conference_id: *conference_id,
                matrix_conference_party_ids: matrix_conference_party_ids.clone(),
                hearing_conference_party_mask: *hearing_conference_party_mask,
                play_mode: play_mode.wire_value(),
            }
            .encode_unchecked(protocol),
            Self::StopAnnouncement { conference_id } => ServerMessage::StopAnnouncement {
                conference_id: *conference_id,
            }
            .encode_unchecked(protocol),
            Self::AnnouncementFinish {
                conference_id,
                play_status,
            } => ServerMessage::AnnouncementFinish {
                conference_id: *conference_id,
                play_status: play_status.wire_value(),
            }
            .encode_unchecked(protocol),
            _ => unreachable!("directly encoded control message"),
        }
    }
}

const fn call_state_precedence(state: CallState) -> u32 {
    match state {
        CallState::OffHook | CallState::Proceed | CallState::Connected | CallState::Transfer => 3,
        CallState::RingOut => 4,
        _ => 2,
    }
}

#[derive(Clone, Copy, Debug)]
struct OpenReceiveParameters {
    packet_ms: u32,
    codec: Codec,
    echo_cancellation: EchoCancellation,
    telephone_event_payload: u8,
    source_address: IpAddr,
    source_port: u16,
}

fn encode_open_receive(
    call: u32,
    party: u32,
    parameters: OpenReceiveParameters,
    encryption: Option<&MediaEncryption>,
    wire: Option<&OpenReceiveChannelWire>,
    protocol: ProtocolVersion,
) -> Result<Vec<u8>, CodecError> {
    let OpenReceiveParameters {
        packet_ms,
        codec,
        echo_cancellation,
        telephone_event_payload,
        source_address,
        source_port,
    } = parameters;
    let conference_id = wire.map_or(call, |value| value.conference_id);
    let g723_bitrate = wire.map_or(0, |value| value.g723_bitrate);
    let stream_passthrough_id = wire.map_or(0, |value| value.stream_passthrough_id);
    let associated_stream_id = wire.map_or(0, |value| value.associated_stream_id);
    let dtmf_type = wire.map_or(10, |value| value.dtmf_type);
    let mixing_mode = wire.map_or(0, |value| value.mixing_mode);
    let direction = wire.map_or(1, |value| value.direction);
    let requested_address_type = wire.map_or_else(
        || u32::from(matches!(source_address, IpAddr::V6(_))),
        |value| value.requested_address_type,
    );
    let encryption = WireEncryptionInfo::from_public(encryption);
    let base = WireOpenReceiveV11 {
        conference_id,
        passthrough_party_id: party,
        packet_millis: packet_ms,
        codec: codec.skinny(),
        vad: echo_cancellation.wire_value(),
        g723_bitrate,
        call_reference: call,
        encryption,
        stream_passthrough_id,
        associated_stream_id,
        rfc2833_payload: u32::from(telephone_event_payload),
        dtmf_type,
    };
    match protocol.wire() {
        21.. => encode(
            wire_id::OPEN_RECEIVE_CHANNEL,
            &WireOpenReceiveV21 {
                base: WireOpenReceiveV18 {
                    base: WireOpenReceiveV17 {
                        base: WireOpenReceiveAddressed {
                            base,
                            mixing_mode,
                            direction,
                            remote: WireExtendedAddress::from_ip(source_address),
                            remote_port: u32::from(source_port),
                        },
                        requested_address_type,
                    },
                    audio_level_adjustment: wire.map_or(0, |value| value.audio_level_adjustment),
                },
                latent_capabilities: WireLatentCapabilities {
                    bytes: wire.map_or([0; 36], |value| value.latent_capabilities),
                },
            },
        ),
        18..=20 => encode(
            wire_id::OPEN_RECEIVE_CHANNEL,
            &WireOpenReceiveV18 {
                base: WireOpenReceiveV17 {
                    base: WireOpenReceiveAddressed {
                        base,
                        mixing_mode,
                        direction,
                        remote: WireExtendedAddress::from_ip(source_address),
                        remote_port: u32::from(source_port),
                    },
                    requested_address_type,
                },
                audio_level_adjustment: wire.map_or(0, |value| value.audio_level_adjustment),
            },
        ),
        17 => encode(
            wire_id::OPEN_RECEIVE_CHANNEL,
            &WireOpenReceiveV17 {
                base: WireOpenReceiveAddressed {
                    base,
                    mixing_mode,
                    direction,
                    remote: WireExtendedAddress::from_ip(source_address),
                    remote_port: u32::from(source_port),
                },
                requested_address_type,
            },
        ),
        version => {
            let remote = WireIpv4Address::from_ip(
                source_address,
                wire_id::OPEN_RECEIVE_CHANNEL,
                "IP address family for pre-v17 protocol",
            )?;
            match version {
                12.. => encode(
                    wire_id::OPEN_RECEIVE_CHANNEL,
                    &WireOpenReceiveV12 {
                        base,
                        mixing_mode,
                        direction,
                        remote,
                        remote_port: u32::from(source_port),
                    },
                ),
                _ => encode(wire_id::OPEN_RECEIVE_CHANNEL, &base),
            }
        }
    }
}

struct StartMediaParameters {
    endpoint: MediaEndpoint,
    silence_suppression: SilenceSuppression,
    traffic_class: MediaTrafficClass,
}

fn encode_start_media(
    call: u32,
    party: u32,
    parameters: StartMediaParameters,
    encryption: Option<&MediaEncryption>,
    wire: Option<&StartMediaTransmissionWire>,
    protocol: ProtocolVersion,
) -> Result<Vec<u8>, CodecError> {
    let StartMediaParameters {
        endpoint,
        silence_suppression,
        traffic_class,
    } = parameters;
    let conference_id = wire.map_or(call, |value| value.conference_id);
    let precedence = u32::from(traffic_class);
    let g723_bitrate = wire.map_or(0, |value| value.g723_bitrate);
    let stream_passthrough_id = wire.map_or(0, |value| value.stream_passthrough_id);
    let associated_stream_id = wire.map_or(0, |value| value.associated_stream_id);
    let dtmf_type = wire.map_or(10, |value| value.dtmf_type);
    let mixing_mode = wire.map_or(0, |value| value.mixing_mode);
    let direction = wire.map_or(1, |value| value.direction);
    let encryption = WireEncryptionInfo::from_public(encryption);
    match protocol.wire() {
        21.. => encode(
            wire_id::START_MEDIA_TRANSMISSION,
            &WireStartMediaV21 {
                base: WireStartMediaV17 {
                    base: WireStartMediaBase {
                        conference_id,
                        passthrough_party_id: party,
                        remote: WireExtendedAddress::from_ip(endpoint.address),
                        remote_port: u32::from(endpoint.rtp_port),
                        packet_millis: endpoint.packet_ms,
                        codec: endpoint.codec.skinny(),
                        precedence,
                        silence_suppression: silence_suppression.wire_value(),
                        max_frames_per_packet: endpoint.max_frames_per_packet,
                        g723_bitrate,
                        call_reference: call,
                        encryption,
                        stream_passthrough_id,
                        associated_stream_id,
                        rfc2833_payload: u32::from(endpoint.telephone_event_payload),
                        dtmf_type,
                    },
                    mixing_mode,
                    direction,
                },
                latent_capabilities: WireLatentCapabilities {
                    bytes: wire.map_or([0; 36], |value| value.latent_capabilities),
                },
            },
        ),
        17..=20 => encode(
            wire_id::START_MEDIA_TRANSMISSION,
            &WireStartMediaV17 {
                base: WireStartMediaBase {
                    conference_id,
                    passthrough_party_id: party,
                    remote: WireExtendedAddress::from_ip(endpoint.address),
                    remote_port: u32::from(endpoint.rtp_port),
                    packet_millis: endpoint.packet_ms,
                    codec: endpoint.codec.skinny(),
                    precedence,
                    silence_suppression: silence_suppression.wire_value(),
                    max_frames_per_packet: endpoint.max_frames_per_packet,
                    g723_bitrate,
                    call_reference: call,
                    encryption,
                    stream_passthrough_id,
                    associated_stream_id,
                    rfc2833_payload: u32::from(endpoint.telephone_event_payload),
                    dtmf_type,
                },
                mixing_mode,
                direction,
            },
        ),
        version => {
            let base = WireStartMediaV11 {
                conference_id,
                passthrough_party_id: party,
                remote: WireIpv4Address::from_ip(
                    endpoint.address,
                    wire_id::START_MEDIA_TRANSMISSION,
                    "IP address family for pre-v17 protocol",
                )?,
                remote_port: u32::from(endpoint.rtp_port),
                packet_millis: endpoint.packet_ms,
                codec: endpoint.codec.skinny(),
                precedence,
                silence_suppression: silence_suppression.wire_value(),
                max_frames_per_packet: endpoint.max_frames_per_packet,
                g723_bitrate,
                call_reference: call,
                encryption,
                stream_passthrough_id,
                associated_stream_id,
                rfc2833_payload: u32::from(endpoint.telephone_event_payload),
                dtmf_type,
            };
            match version {
                12.. => encode(
                    wire_id::START_MEDIA_TRANSMISSION,
                    &WireStartMediaV12 {
                        base,
                        mixing_mode,
                        direction,
                    },
                ),
                _ => encode(wire_id::START_MEDIA_TRANSMISSION, &base),
            }
        }
    }
}

fn encode_start_multicast_reception(
    message: &MulticastMediaReception,
    protocol: ProtocolVersion,
) -> Result<Vec<u8>, CodecError> {
    match protocol.wire() {
        17.. => encode(
            wire_id::START_MULTICAST_MEDIA_RECEPTION,
            &WireStartMulticastReception::<WireExtendedAddress> {
                conference_id: message.conference_id.get(),
                passthrough_party_id: message.passthrough_party_id.get(),
                address: WireExtendedAddress::from_ip(message.address),
                port: u32::from(message.port),
                packet_millis: message.packet_millis,
                codec: message.codec.wire_value(),
                echo_cancellation: message.echo_cancellation.wire_value(),
                g723_bitrate: message.g723_bitrate.wire_value(),
                call_reference: message.call_reference.get(),
            },
        ),
        _ => encode(
            wire_id::START_MULTICAST_MEDIA_RECEPTION,
            &WireStartMulticastReception::<WireIpv4Address> {
                conference_id: message.conference_id.get(),
                passthrough_party_id: message.passthrough_party_id.get(),
                address: WireIpv4Address::from_ip(
                    message.address,
                    wire_id::START_MULTICAST_MEDIA_RECEPTION,
                    "IP address family for pre-v17 protocol",
                )?,
                port: u32::from(message.port),
                packet_millis: message.packet_millis,
                codec: message.codec.wire_value(),
                echo_cancellation: message.echo_cancellation.wire_value(),
                g723_bitrate: message.g723_bitrate.wire_value(),
                call_reference: message.call_reference.get(),
            },
        ),
    }
}

fn decode_start_multicast_reception(
    payload: &[u8],
    protocol: ProtocolVersion,
    message_id: u32,
) -> Result<ServerMessage, CodecError> {
    let (conference_id, party_id, address, port, packet_millis, codec, echo, g723, call_reference) =
        match protocol.wire() {
            17.. => {
                validate_exact_payload(payload, message_id, 52)?;
                let value: WireStartMulticastReception<WireExtendedAddress> =
                    decode(message_id, payload)?;
                (
                    value.conference_id,
                    value.passthrough_party_id,
                    value.address.to_ip(message_id)?,
                    value.port,
                    value.packet_millis,
                    value.codec,
                    value.echo_cancellation,
                    value.g723_bitrate,
                    value.call_reference,
                )
            }
            _ => {
                validate_exact_payload(payload, message_id, 36)?;
                let value: WireStartMulticastReception<WireIpv4Address> =
                    decode(message_id, payload)?;
                (
                    value.conference_id,
                    value.passthrough_party_id,
                    value.address.to_ip(message_id)?,
                    value.port,
                    value.packet_millis,
                    value.codec,
                    value.echo_cancellation,
                    value.g723_bitrate,
                    value.call_reference,
                )
            }
        };
    Ok(ServerMessage::StartMulticastMediaReception(
        MulticastMediaReception {
            conference_id: conference_id.into(),
            passthrough_party_id: party_id.into(),
            call_reference: call_reference.into(),
            address,
            port: decode_port(port, message_id, "multicast port")?,
            packet_millis,
            codec: Codec::from(codec),
            echo_cancellation: EchoCancellation::from(echo),
            g723_bitrate: G723BitRate::from(g723),
        },
    ))
}

fn encode_start_multicast_transmission(
    message: &MulticastMediaTransmission,
    protocol: ProtocolVersion,
) -> Result<Vec<u8>, CodecError> {
    match protocol.wire() {
        17.. => encode(
            wire_id::START_MULTICAST_MEDIA_TRANSMISSION,
            &WireStartMulticastTransmission::<WireExtendedAddress> {
                conference_id: message.conference_id.get(),
                passthrough_party_id: message.passthrough_party_id.get(),
                address: WireExtendedAddress::from_ip(message.address),
                port: u32::from(message.port),
                packet_millis: message.packet_millis,
                codec: message.codec.wire_value(),
                precedence: message.precedence,
                silence_suppression: message.silence_suppression,
                max_frames_per_packet: message.max_frames_per_packet,
                g723_bitrate: message.g723_bitrate.wire_value(),
                call_reference: message.call_reference.get(),
            },
        ),
        _ => encode(
            wire_id::START_MULTICAST_MEDIA_TRANSMISSION,
            &WireStartMulticastTransmission::<WireIpv4Address> {
                conference_id: message.conference_id.get(),
                passthrough_party_id: message.passthrough_party_id.get(),
                address: WireIpv4Address::from_ip(
                    message.address,
                    wire_id::START_MULTICAST_MEDIA_TRANSMISSION,
                    "IP address family for pre-v17 protocol",
                )?,
                port: u32::from(message.port),
                packet_millis: message.packet_millis,
                codec: message.codec.wire_value(),
                precedence: message.precedence,
                silence_suppression: message.silence_suppression,
                max_frames_per_packet: message.max_frames_per_packet,
                g723_bitrate: message.g723_bitrate.wire_value(),
                call_reference: message.call_reference.get(),
            },
        ),
    }
}

fn decode_start_multicast_transmission(
    payload: &[u8],
    protocol: ProtocolVersion,
    message_id: u32,
) -> Result<ServerMessage, CodecError> {
    let (
        conference_id,
        party_id,
        address,
        port,
        packet_millis,
        codec,
        precedence,
        silence,
        max_frames,
        g723,
        call_reference,
    ) = match protocol.wire() {
        17.. => {
            validate_exact_payload(payload, message_id, 60)?;
            let value: WireStartMulticastTransmission<WireExtendedAddress> =
                decode(message_id, payload)?;
            (
                value.conference_id,
                value.passthrough_party_id,
                value.address.to_ip(message_id)?,
                value.port,
                value.packet_millis,
                value.codec,
                value.precedence,
                value.silence_suppression,
                value.max_frames_per_packet,
                value.g723_bitrate,
                value.call_reference,
            )
        }
        _ => {
            validate_exact_payload(payload, message_id, 44)?;
            let value: WireStartMulticastTransmission<WireIpv4Address> =
                decode(message_id, payload)?;
            (
                value.conference_id,
                value.passthrough_party_id,
                value.address.to_ip(message_id)?,
                value.port,
                value.packet_millis,
                value.codec,
                value.precedence,
                value.silence_suppression,
                value.max_frames_per_packet,
                value.g723_bitrate,
                value.call_reference,
            )
        }
    };
    Ok(ServerMessage::StartMulticastMediaTransmission(
        MulticastMediaTransmission {
            conference_id: conference_id.into(),
            passthrough_party_id: party_id.into(),
            call_reference: call_reference.into(),
            address,
            port: decode_port(port, message_id, "multicast port")?,
            packet_millis,
            codec: Codec::from(codec),
            precedence,
            silence_suppression: silence,
            max_frames_per_packet: max_frames,
            g723_bitrate: G723BitRate::from(g723),
        },
    ))
}

fn decode_open_receive(
    payload: &[u8],
    protocol: ProtocolVersion,
    message_id: u32,
) -> Result<ServerMessage, CodecError> {
    let (
        call_reference,
        passthrough_party_id,
        packet_ms,
        codec,
        echo,
        rfc2833,
        source_address,
        source_port,
        encryption,
        wire,
    ) = match protocol.wire() {
        21.. => {
            let value: WireOpenReceiveV21 = decode(message_id, payload)?;
            (
                value.base.base.base.base.call_reference,
                value.base.base.base.base.passthrough_party_id,
                value.base.base.base.base.packet_millis,
                value.base.base.base.base.codec,
                value.base.base.base.base.vad,
                value.base.base.base.base.rfc2833_payload,
                value.base.base.base.remote.to_ip(message_id)?,
                decode_port(
                    value.base.base.base.remote_port,
                    message_id,
                    "source RTP port",
                )?,
                value.base.base.base.base.encryption,
                OpenReceiveChannelWire {
                    conference_id: value.base.base.base.base.conference_id,
                    g723_bitrate: value.base.base.base.base.g723_bitrate,
                    stream_passthrough_id: value.base.base.base.base.stream_passthrough_id,
                    associated_stream_id: value.base.base.base.base.associated_stream_id,
                    dtmf_type: value.base.base.base.base.dtmf_type,
                    mixing_mode: value.base.base.base.mixing_mode,
                    direction: value.base.base.base.direction,
                    requested_address_type: value.base.base.requested_address_type,
                    audio_level_adjustment: value.base.audio_level_adjustment,
                    latent_capabilities: value.latent_capabilities.bytes,
                },
            )
        }
        18..=20 => {
            let value: WireOpenReceiveV18 = decode(message_id, payload)?;
            (
                value.base.base.base.call_reference,
                value.base.base.base.passthrough_party_id,
                value.base.base.base.packet_millis,
                value.base.base.base.codec,
                value.base.base.base.vad,
                value.base.base.base.rfc2833_payload,
                value.base.base.remote.to_ip(message_id)?,
                decode_port(value.base.base.remote_port, message_id, "source RTP port")?,
                value.base.base.base.encryption,
                OpenReceiveChannelWire {
                    conference_id: value.base.base.base.conference_id,
                    g723_bitrate: value.base.base.base.g723_bitrate,
                    stream_passthrough_id: value.base.base.base.stream_passthrough_id,
                    associated_stream_id: value.base.base.base.associated_stream_id,
                    dtmf_type: value.base.base.base.dtmf_type,
                    mixing_mode: value.base.base.mixing_mode,
                    direction: value.base.base.direction,
                    requested_address_type: value.base.requested_address_type,
                    audio_level_adjustment: value.audio_level_adjustment,
                    latent_capabilities: [0; 36],
                },
            )
        }
        17 => {
            let value: WireOpenReceiveV17 = decode(message_id, payload)?;
            (
                value.base.base.call_reference,
                value.base.base.passthrough_party_id,
                value.base.base.packet_millis,
                value.base.base.codec,
                value.base.base.vad,
                value.base.base.rfc2833_payload,
                value.base.remote.to_ip(message_id)?,
                decode_port(value.base.remote_port, message_id, "source RTP port")?,
                value.base.base.encryption,
                OpenReceiveChannelWire {
                    conference_id: value.base.base.conference_id,
                    g723_bitrate: value.base.base.g723_bitrate,
                    stream_passthrough_id: value.base.base.stream_passthrough_id,
                    associated_stream_id: value.base.base.associated_stream_id,
                    dtmf_type: value.base.base.dtmf_type,
                    mixing_mode: value.base.mixing_mode,
                    direction: value.base.direction,
                    requested_address_type: value.requested_address_type,
                    audio_level_adjustment: 0,
                    latent_capabilities: [0; 36],
                },
            )
        }
        12..=16 => {
            let value: WireOpenReceiveV12 = decode(message_id, payload)?;
            (
                value.base.call_reference,
                value.base.passthrough_party_id,
                value.base.packet_millis,
                value.base.codec,
                value.base.vad,
                value.base.rfc2833_payload,
                value.remote.to_ip(message_id)?,
                decode_port(value.remote_port, message_id, "source RTP port")?,
                value.base.encryption,
                OpenReceiveChannelWire {
                    conference_id: value.base.conference_id,
                    g723_bitrate: value.base.g723_bitrate,
                    stream_passthrough_id: value.base.stream_passthrough_id,
                    associated_stream_id: value.base.associated_stream_id,
                    dtmf_type: value.base.dtmf_type,
                    mixing_mode: value.mixing_mode,
                    direction: value.direction,
                    requested_address_type: 0,
                    audio_level_adjustment: 0,
                    latent_capabilities: [0; 36],
                },
            )
        }
        _ => {
            let value: WireOpenReceiveV11 = decode(message_id, payload)?;
            (
                value.call_reference,
                value.passthrough_party_id,
                value.packet_millis,
                value.codec,
                value.vad,
                value.rfc2833_payload,
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                0,
                value.encryption,
                OpenReceiveChannelWire {
                    conference_id: value.conference_id,
                    g723_bitrate: value.g723_bitrate,
                    stream_passthrough_id: value.stream_passthrough_id,
                    associated_stream_id: value.associated_stream_id,
                    dtmf_type: value.dtmf_type,
                    mixing_mode: 0,
                    direction: 0,
                    requested_address_type: 0,
                    audio_level_adjustment: 0,
                    latent_capabilities: [0; 36],
                },
            )
        }
    };
    let telephone_event_payload = u8::try_from(rfc2833).map_err(|_| CodecError::InvalidValue {
        message_id,
        field: "RFC2833 payload",
        value: u64::from(rfc2833),
    })?;
    Ok(ServerMessage::OpenReceiveChannel {
        call_reference,
        passthrough_party_id,
        packet_ms,
        codec: Codec::from(codec),
        echo_cancellation: EchoCancellation::from(echo),
        telephone_event_payload,
        source_address,
        source_port,
        encryption: encryption.to_public(message_id)?,
        wire: (wire != canonical_open_receive_wire(call_reference, source_address, protocol))
            .then_some(wire),
    })
}

fn decode_start_media(
    payload: &[u8],
    protocol: ProtocolVersion,
    message_id: u32,
) -> Result<ServerMessage, CodecError> {
    let (
        call_reference,
        passthrough_party_id,
        address,
        port,
        packet_ms,
        codec,
        precedence,
        silence_suppression,
        max_frames_per_packet,
        rfc2833,
        encryption,
        wire,
    ) = match protocol.wire() {
        21.. => {
            let value: WireStartMediaV21 = decode(message_id, payload)?;
            (
                value.base.base.call_reference,
                value.base.base.passthrough_party_id,
                value.base.base.remote.to_ip(message_id)?,
                value.base.base.remote_port,
                value.base.base.packet_millis,
                value.base.base.codec,
                value.base.base.precedence,
                value.base.base.silence_suppression,
                value.base.base.max_frames_per_packet,
                value.base.base.rfc2833_payload,
                value.base.base.encryption,
                StartMediaTransmissionWire {
                    conference_id: value.base.base.conference_id,
                    g723_bitrate: value.base.base.g723_bitrate,
                    stream_passthrough_id: value.base.base.stream_passthrough_id,
                    associated_stream_id: value.base.base.associated_stream_id,
                    dtmf_type: value.base.base.dtmf_type,
                    mixing_mode: value.base.mixing_mode,
                    direction: value.base.direction,
                    latent_capabilities: value.latent_capabilities.bytes,
                },
            )
        }
        17..=20 => {
            let value: WireStartMediaV17 = decode(message_id, payload)?;
            (
                value.base.call_reference,
                value.base.passthrough_party_id,
                value.base.remote.to_ip(message_id)?,
                value.base.remote_port,
                value.base.packet_millis,
                value.base.codec,
                value.base.precedence,
                value.base.silence_suppression,
                value.base.max_frames_per_packet,
                value.base.rfc2833_payload,
                value.base.encryption,
                StartMediaTransmissionWire {
                    conference_id: value.base.conference_id,
                    g723_bitrate: value.base.g723_bitrate,
                    stream_passthrough_id: value.base.stream_passthrough_id,
                    associated_stream_id: value.base.associated_stream_id,
                    dtmf_type: value.base.dtmf_type,
                    mixing_mode: value.mixing_mode,
                    direction: value.direction,
                    latent_capabilities: [0; 36],
                },
            )
        }
        12..=16 => {
            let value: WireStartMediaV12 = decode(message_id, payload)?;
            (
                value.base.call_reference,
                value.base.passthrough_party_id,
                value.base.remote.to_ip(message_id)?,
                value.base.remote_port,
                value.base.packet_millis,
                value.base.codec,
                value.base.precedence,
                value.base.silence_suppression,
                value.base.max_frames_per_packet,
                value.base.rfc2833_payload,
                value.base.encryption,
                StartMediaTransmissionWire {
                    conference_id: value.base.conference_id,
                    g723_bitrate: value.base.g723_bitrate,
                    stream_passthrough_id: value.base.stream_passthrough_id,
                    associated_stream_id: value.base.associated_stream_id,
                    dtmf_type: value.base.dtmf_type,
                    mixing_mode: value.mixing_mode,
                    direction: value.direction,
                    latent_capabilities: [0; 36],
                },
            )
        }
        _ => {
            let value: WireStartMediaV11 = decode(message_id, payload)?;
            (
                value.call_reference,
                value.passthrough_party_id,
                value.remote.to_ip(message_id)?,
                value.remote_port,
                value.packet_millis,
                value.codec,
                value.precedence,
                value.silence_suppression,
                value.max_frames_per_packet,
                value.rfc2833_payload,
                value.encryption,
                StartMediaTransmissionWire {
                    conference_id: value.conference_id,
                    g723_bitrate: value.g723_bitrate,
                    stream_passthrough_id: value.stream_passthrough_id,
                    associated_stream_id: value.associated_stream_id,
                    dtmf_type: value.dtmf_type,
                    mixing_mode: 0,
                    direction: 0,
                    latent_capabilities: [0; 36],
                },
            )
        }
    };
    let rtp_port = decode_port(port, message_id, "RTP port")?;
    let telephone_event_payload = u8::try_from(rfc2833).map_err(|_| CodecError::InvalidValue {
        message_id,
        field: "RFC2833 payload",
        value: u64::from(rfc2833),
    })?;
    Ok(ServerMessage::StartMediaTransmission {
        call_reference,
        passthrough_party_id,
        endpoint: MediaEndpoint {
            address,
            rtp_port,
            rtcp_port: rtp_port.saturating_add(1),
            codec: Codec::from(codec),
            packet_ms,
            max_frames_per_packet,
            telephone_event_payload,
        },
        silence_suppression: SilenceSuppression::from(silence_suppression),
        traffic_class: MediaTrafficClass::from_wire(u8::try_from(precedence).map_err(|_| {
            CodecError::InvalidValue {
                message_id,
                field: "media traffic class",
                value: u64::from(precedence),
            }
        })?),
        encryption: encryption.to_public(message_id)?,
        wire: (wire != canonical_start_media_wire(call_reference, protocol)).then_some(wire),
    })
}

#[cfg(test)]
mod tests {
    use super::catalog::MessageDirection;
    use super::values::SoftKey;
    use super::wire::{FrameDecoder, MAX_FRAME_SIZE};
    use super::*;

    fn fixture(source: &str) -> Vec<u8> {
        source
            .split_whitespace()
            .map(|byte| u8::from_str_radix(byte, 16).expect("valid fixture byte"))
            .collect()
    }

    fn deterministic_payload(message_id: u32, protocol: u32, length: usize) -> Vec<u8> {
        let mut state = u64::from(message_id)
            ^ (u64::from(protocol) << 32)
            ^ (length as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        (0..length)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    fn fuzz_lengths() -> impl Iterator<Item = usize> {
        (0..=96).chain([127, 255, 511, 1024, MAX_FRAME_SIZE - 12])
    }

    const fn test_rtp_payload_number(value: u32) -> RtpPayloadNumber {
        match RtpPayloadNumber::new(value) {
            Ok(value) => value,
            Err(_) => panic!("test RTP payload number is out of range"),
        }
    }

    fn typed_video_payload(arm: MultimediaVideoCapabilityArm) -> MultimediaPayload {
        let payload_number = match arm.codec() {
            Codec::H261 => 31,
            Codec::H263 => 34,
            Codec::H263Plus => 96,
            Codec::H264 => 97,
            _ => unreachable!("typed video arms always have a modeled codec"),
        };
        MultimediaPayload::new(
            test_rtp_payload_number(payload_number),
            MultimediaVideoCapability::new(
                1_024,
                [
                    MultimediaPictureFormat {
                        format: VideoFormat::Cif4,
                        minimum_picture_interval: 1,
                    },
                    MultimediaPictureFormat {
                        format: VideoFormat::Cif,
                        minimum_picture_interval: 2,
                    },
                ],
                7,
                arm,
            )
            .unwrap(),
        )
    }

    #[test]
    fn every_catalogued_client_decoder_is_panic_free_for_bounded_property_corpus() {
        let protocols = [
            ProtocolVersion::V3,
            ProtocolVersion::V8,
            ProtocolVersion::V17,
            ProtocolVersion::V22,
        ];
        let mut cases = 0_usize;
        for message_id in MessageId::ALL_KNOWN
            .iter()
            .copied()
            .filter(|id| id.direction() == Some(MessageDirection::DeviceToServer))
        {
            for protocol in protocols {
                for length in fuzz_lengths() {
                    let frame = Frame::new(
                        protocol.wire(),
                        message_id.wire_value(),
                        deterministic_payload(message_id.wire_value(), protocol.wire(), length),
                    );
                    let _ = ClientMessage::decode_with_version(frame, protocol);
                    cases += 1;
                }
            }
        }
        assert!(
            cases > 20_000,
            "property corpus unexpectedly shrank: {cases}"
        );
    }

    #[test]
    fn every_catalogued_server_encoder_round_trips_all_decodable_bounded_inputs() {
        let protocols = [
            ProtocolVersion::V3,
            ProtocolVersion::V8,
            ProtocolVersion::V17,
            ProtocolVersion::V22,
        ];
        let mut decoded = 0_usize;
        let mut encoded = 0_usize;
        for message_id in MessageId::ALL_KNOWN
            .iter()
            .copied()
            .filter(|id| id.direction() == Some(MessageDirection::ServerToDevice))
        {
            for protocol in protocols {
                for length in fuzz_lengths() {
                    let frame = Frame::new(
                        protocol.wire(),
                        message_id.wire_value(),
                        deterministic_payload(message_id.wire_value(), protocol.wire(), length),
                    );
                    let Ok(message) = ServerMessage::decode(frame, protocol) else {
                        continue;
                    };
                    decoded += 1;
                    let Ok(bytes) = message.encode(protocol) else {
                        continue;
                    };
                    assert!(bytes.len() <= MAX_FRAME_SIZE);
                    let frames = FrameDecoder::new().push(&bytes).unwrap();
                    assert_eq!(frames.len(), 1);
                    assert_eq!(
                        ServerMessage::decode(frames.into_iter().next().unwrap(), protocol)
                            .unwrap(),
                        message
                    );
                    encoded += 1;
                }
            }
        }
        assert!(
            decoded > 1_000,
            "decodable encoder corpus unexpectedly shrank: {decoded}"
        );
        assert!(
            encoded > 1_000,
            "encodable property corpus unexpectedly shrank: {encoded}"
        );
    }

    #[test]
    fn registration_preserves_both_reported_address_families() {
        let message = ClientMessage::Register(RegistrationMessage {
            device_id: DeviceId::new("SEP001122334455").unwrap(),
            reported_address: Some(Ipv4Addr::new(192, 0, 2, 10)),
            reported_ipv6_address: Some("2001:db8::10".parse().unwrap()),
            device_type: DeviceType::Cisco7962,
            advertised_protocol: Some(ProtocolVersion::V22.wire()),
            features: PhoneFeatures::empty(),
            firmware: "test-load".into(),
            configuration_version_stamp: BoundedBytes::default(),
            wire: Some(RegistrationWireDetails {
                layout: RegistrationWireLayout::default(),
                station_user_id: 17,
                station_instance: 2,
                max_streams: 5,
                active_streams: 1,
                mac_address_and_padding: [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0, 0, 0, 0, 0, 0],
                max_conferences: 3,
                active_conferences: 1,
                ipv4_address_scope: 3,
                max_lines: 6,
                ipv6_address_scope: 2,
            }),
        });
        let bytes = message.encode(ProtocolVersion::V22).unwrap();
        let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
        assert_eq!(
            ClientMessage::decode_with_version(frame, ProtocolVersion::V22).unwrap(),
            message
        );
    }

    #[test]
    fn registration_preserves_every_complete_canonical_prefix() {
        let mut payload = [0_u8; REGISTER_CANONICAL_BYTES];
        let device_id = b"SEP001122334455";
        payload[..device_id.len()].copy_from_slice(device_id);
        payload[16..20].copy_from_slice(&17_u32.to_le_bytes());
        payload[20..24].copy_from_slice(&2_u32.to_le_bytes());
        payload[24..28].copy_from_slice(&[192, 0, 2, 25]);
        payload[28..32].copy_from_slice(&DeviceType::Cisco7925.wire_value().to_le_bytes());
        payload[32..36].copy_from_slice(&5_u32.to_le_bytes());
        payload[36..40].copy_from_slice(&1_u32.to_le_bytes());
        payload[40..44].copy_from_slice(&ProtocolVersion::V11.wire().to_le_bytes());
        payload[44..48].copy_from_slice(&3_u32.to_le_bytes());
        payload[48..52].copy_from_slice(&1_u32.to_le_bytes());
        payload[52..64].copy_from_slice(&[0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 1, 2, 3, 4, 5, 6]);
        payload[64..68].copy_from_slice(&3_u32.to_le_bytes());
        payload[68..72].copy_from_slice(&6_u32.to_le_bytes());
        payload[72..88].copy_from_slice(&Ipv6Addr::LOCALHOST.octets());
        payload[88..92].copy_from_slice(&2_u32.to_le_bytes());
        payload[92..101].copy_from_slice(b"SCCP-test");

        for prefix_bytes in [36, 40, 44, 48, 52, 64, 68, 72, 88, 92, 124] {
            let expected_payload = payload[..prefix_bytes].to_vec();
            let message = ClientMessage::decode_with_version(
                Frame::new(0, wire_id::REGISTER, expected_payload.clone()),
                ProtocolVersion::V22,
            )
            .unwrap();
            let ClientMessage::Register(registration) = &message else {
                unreachable!("registration frame decoded as another message")
            };
            assert_eq!(registration.device_type, DeviceType::Cisco7925);
            assert_eq!(
                registration.advertised_protocol,
                (prefix_bytes >= 44).then_some(ProtocolVersion::V11.wire())
            );
            assert_eq!(
                registration.reported_ipv6_address,
                (prefix_bytes >= 88).then_some(Ipv6Addr::LOCALHOST)
            );
            assert_eq!(
                registration.firmware,
                if prefix_bytes == REGISTER_CANONICAL_BYTES {
                    "SCCP-test"
                } else {
                    ""
                }
            );
            assert!(matches!(
                registration.wire,
                Some(RegistrationWireDetails {
                    layout: RegistrationWireLayout::Canonical {
                        prefix_bytes: actual
                    },
                    ..
                }) if usize::from(actual) == prefix_bytes
            ));

            let encoded = message.encode(ProtocolVersion::V22).unwrap();
            let encoded_frame = FrameDecoder::new().push(&encoded).unwrap().remove(0);
            assert_eq!(encoded_frame.payload, expected_payload);
        }
    }

    #[test]
    fn registration_preserves_alternate_32_byte_layout() {
        let message = ClientMessage::Register(RegistrationMessage {
            device_id: DeviceId::new("SEP001122334455").unwrap(),
            reported_address: None,
            reported_ipv6_address: None,
            device_type: DeviceType::Cisco7920,
            advertised_protocol: Some(ProtocolVersion::V3.wire()),
            features: PhoneFeatures::empty(),
            firmware: String::new(),
            configuration_version_stamp: BoundedBytes::default(),
            wire: Some(RegistrationWireDetails {
                layout: RegistrationWireLayout::Alternate32,
                station_user_id: 17,
                station_instance: 2,
                max_streams: 0,
                active_streams: 0,
                mac_address_and_padding: [0; 12],
                max_conferences: 0,
                active_conferences: 0,
                ipv4_address_scope: 0,
                max_lines: 0,
                ipv6_address_scope: 0,
            }),
        });
        let bytes = message.encode(ProtocolVersion::V22).unwrap();
        let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
        assert_eq!(frame.payload.len(), REGISTER_ALTERNATE_BYTES);
        assert_eq!(
            ClientMessage::decode_with_version(frame, ProtocolVersion::V22).unwrap(),
            message
        );
    }

    #[test]
    fn short_zero_protocol_registration_uses_legacy_fallback_and_round_trips() {
        for payload_bytes in [32, 44, 48] {
            let mut payload = vec![0_u8; payload_bytes];
            let device_id = b"SEP001122334455";
            payload[..device_id.len()].copy_from_slice(device_id);
            payload[28..32].copy_from_slice(&DeviceType::Cisco7925.wire_value().to_le_bytes());

            let message = ClientMessage::decode_with_version(
                Frame::new(0, wire_id::REGISTER, payload.clone()),
                ProtocolVersion::V22,
            )
            .unwrap();
            let ClientMessage::Register(registration) = &message else {
                unreachable!("registration frame decoded as another message")
            };
            assert_eq!(registration.advertised_protocol, None);

            let encoded = message.encode(ProtocolVersion::V22).unwrap();
            let encoded_frame = FrameDecoder::new().push(&encoded).unwrap().remove(0);
            assert_eq!(encoded_frame.payload, payload);
        }
    }

    #[test]
    fn registration_encoder_rejects_values_omitted_by_selected_prefix() {
        let mut message = ClientMessage::Register(RegistrationMessage {
            device_id: DeviceId::new("SEP001122334455").unwrap(),
            reported_address: Some(Ipv4Addr::LOCALHOST),
            reported_ipv6_address: None,
            device_type: DeviceType::Cisco7925,
            advertised_protocol: None,
            features: PhoneFeatures::empty(),
            firmware: String::new(),
            configuration_version_stamp: BoundedBytes::default(),
            wire: Some(RegistrationWireDetails {
                layout: RegistrationWireLayout::Canonical { prefix_bytes: 36 },
                station_user_id: 0,
                station_instance: 1,
                max_streams: 5,
                active_streams: 0,
                mac_address_and_padding: [0; 12],
                max_conferences: 0,
                active_conferences: 0,
                ipv4_address_scope: 0,
                max_lines: 0,
                ipv6_address_scope: 0,
            }),
        });
        assert!(message.encode(ProtocolVersion::V22).is_ok());

        let ClientMessage::Register(registration) = &mut message else {
            unreachable!("test message is registration")
        };
        registration.wire.as_mut().unwrap().active_streams = 1;
        assert!(matches!(
            message.encode(ProtocolVersion::V22),
            Err(CodecError::InvalidValue {
                field: "active streams",
                ..
            })
        ));
    }

    #[test]
    fn registration_preserves_every_bounded_configuration_suffix() {
        let base = ClientMessage::Register(RegistrationMessage {
            device_id: DeviceId::new("SEP001122334455").unwrap(),
            reported_address: None,
            reported_ipv6_address: None,
            device_type: DeviceType::Cisco7962,
            advertised_protocol: Some(ProtocolVersion::V22.wire()),
            features: PhoneFeatures::empty(),
            firmware: "test-load".into(),
            configuration_version_stamp: BoundedBytes::default(),
            wire: Some(RegistrationWireDetails {
                layout: RegistrationWireLayout::default(),
                station_user_id: 0,
                station_instance: 1,
                max_streams: 0,
                active_streams: 0,
                mac_address_and_padding: [0; 12],
                max_conferences: 0,
                active_conferences: 0,
                ipv4_address_scope: 0,
                max_lines: 0,
                ipv6_address_scope: 0,
            }),
        });

        for length in 0..=48 {
            let mut message = base.clone();
            let ClientMessage::Register(registration) = &mut message else {
                unreachable!("test message is registration")
            };
            registration.configuration_version_stamp =
                BoundedBytes::try_from(vec![0xa5; length]).unwrap();
            let bytes = message.encode(ProtocolVersion::V22).unwrap();
            let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
            assert_eq!(frame.payload.len(), 124 + length);
            assert_eq!(
                ClientMessage::decode_with_version(frame, ProtocolVersion::V22).unwrap(),
                message
            );
        }
    }

    #[test]
    fn registration_rejects_unsupported_payload_lengths() {
        for invalid_length in [33, 35, 37, 56, 76, 96, 123] {
            assert!(matches!(
                ClientMessage::decode_with_version(
                    Frame::new(0, wire_id::REGISTER, vec![0; invalid_length]),
                    ProtocolVersion::V22,
                ),
                Err(CodecError::InvalidValue {
                    field: "registration payload length",
                    ..
                })
            ));
        }
        assert!(matches!(
            ClientMessage::decode_with_version(
                Frame::new(0, wire_id::REGISTER, vec![0; 31]),
                ProtocolVersion::V22,
            ),
            Err(CodecError::Truncated { needed: 32, .. })
        ));
        assert!(matches!(
            ClientMessage::decode_with_version(
                Frame::new(0, wire_id::REGISTER, vec![0; 173]),
                ProtocolVersion::V22,
            ),
            Err(CodecError::TrailingBytes { .. })
        ));
    }

    #[test]
    fn alarm_preserves_both_supported_wire_lengths() {
        for parameters in [None, Some([0x1122_3344, 0xaabb_ccdd])] {
            let message = ClientMessage::Alarm {
                severity: AlarmSeverity::Warning,
                text: "TFTP load failed".into(),
                parameters,
            };
            let bytes = message.encode(ProtocolVersion::V17).unwrap();
            assert_eq!(bytes.len(), if parameters.is_some() { 104 } else { 96 });
            let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
            assert_eq!(ClientMessage::decode(frame).unwrap(), message);
        }
    }

    #[test]
    fn capabilities_response_consumes_all_eighteen_fixed_slots() {
        let capabilities = (0_u32..12)
            .map(|index| MediaCapability {
                codec: if index.is_multiple_of(2) {
                    Codec::Pcmu
                } else {
                    Codec::Pcma
                },
                max_packet_ms: index + 1,
                codec_parameters: [index as u8; 8],
            })
            .collect::<Vec<_>>();
        let message = ClientMessage::CapabilitiesResponse(capabilities);

        let bytes = message.encode(ProtocolVersion::V22).unwrap();
        let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);

        assert_eq!(frame.payload.len(), 4 + 18 * 16);
        assert_eq!(ClientMessage::decode(frame).unwrap(), message);
    }

    #[test]
    fn capabilities_response_selects_the_extended_fixed_reservoir() {
        let capabilities = (0_u32..20)
            .map(|index| MediaCapability {
                codec: Codec::Unknown(0x1000 + index),
                max_packet_ms: index + 1,
                codec_parameters: [index as u8; 8],
            })
            .collect::<Vec<_>>();
        let message = ClientMessage::CapabilitiesResponse(capabilities);

        let bytes = message.encode(ProtocolVersion::V22).unwrap();
        let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);

        assert_eq!(frame.payload.len(), 4 + 24 * 16);
        assert_eq!(ClientMessage::decode(frame).unwrap(), message);
    }

    #[test]
    fn capabilities_response_accepts_exact_counted_prefixes() {
        let capabilities = vec![
            MediaCapability {
                codec: Codec::Pcmu,
                max_packet_ms: 2,
                codec_parameters: [1; 8],
            },
            MediaCapability {
                codec: Codec::Pcma,
                max_packet_ms: 3,
                codec_parameters: [2; 8],
            },
        ];
        let mut payload = 2_u32.to_le_bytes().to_vec();
        for capability in &capabilities {
            payload.extend_from_slice(
                &encode(
                    wire_id::CAPABILITIES_RES,
                    &WireMediaCapability {
                        codec: capability.codec.wire_value(),
                        max_frames_per_packet: capability.max_packet_ms,
                        codec_parameters: capability.codec_parameters,
                    },
                )
                .unwrap(),
            );
        }
        assert_eq!(payload.len(), 4 + 2 * 16);
        assert_eq!(
            ClientMessage::decode(Frame::new(
                ProtocolVersion::V22.wire(),
                wire_id::CAPABILITIES_RES,
                payload,
            ))
            .unwrap(),
            ClientMessage::CapabilitiesResponse(capabilities)
        );
    }

    #[test]
    fn capabilities_response_rejects_incomplete_or_mismatched_counted_storage() {
        let mut partial = vec![0; 4 + 19 * 16];
        partial[..4].copy_from_slice(&1_u32.to_le_bytes());
        assert!(matches!(
            ClientMessage::decode(Frame::new(
                ProtocolVersion::V22.wire(),
                wire_id::CAPABILITIES_RES,
                partial,
            )),
            Err(CodecError::TrailingBytes { count: 288, .. })
        ));

        let mut too_many = vec![0; 4 + 18 * 16];
        too_many[..4].copy_from_slice(&19_u32.to_le_bytes());
        assert!(matches!(
            ClientMessage::decode(Frame::new(
                ProtocolVersion::V22.wire(),
                wire_id::CAPABILITIES_RES,
                too_many,
            )),
            Err(CodecError::CountTooLarge {
                field: "audio capabilities",
                maximum: 18,
                ..
            })
        ));
    }

    #[test]
    fn capabilities_response_discards_nonzero_inactive_reservoir_storage() {
        let mut payload = vec![0xa5; 4 + 24 * 16];
        payload[..4].copy_from_slice(&1_u32.to_le_bytes());
        payload[4..20].copy_from_slice(
            &encode(
                wire_id::CAPABILITIES_RES,
                &WireMediaCapability {
                    codec: Codec::Pcmu.wire_value(),
                    max_frames_per_packet: 2,
                    codec_parameters: [1; 8],
                },
            )
            .unwrap(),
        );
        assert_eq!(
            ClientMessage::decode(Frame::new(
                ProtocolVersion::V22.wire(),
                wire_id::CAPABILITIES_RES,
                payload,
            ))
            .unwrap(),
            ClientMessage::CapabilitiesResponse(vec![MediaCapability {
                codec: Codec::Pcmu,
                max_packet_ms: 2,
                codec_parameters: [1; 8],
            }])
        );
    }

    #[test]
    fn enbloc_call_selects_exact_version_and_length_layouts() {
        for (version, line_instance, payload_bytes) in [
            (ProtocolVersion::V3, 0, 24),
            (ProtocolVersion::V17, 2, 28),
            (ProtocolVersion::V22, 2, 32),
        ] {
            let message = ClientMessage::EnblocCall {
                called_party: "2001".into(),
                line_instance,
            };
            let bytes = message.encode(version).unwrap();
            let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
            assert_eq!(frame.payload.len(), payload_bytes);
            assert_eq!(
                ClientMessage::decode_with_version(frame, version).unwrap(),
                message
            );
        }

        let early_with_line = encode(
            wire_id::ENBLOC_CALL,
            &WireEnblocWithLine::<24, 0> {
                called_party: WireAlignedText::new(wire_id::ENBLOC_CALL, "called party", "2001")
                    .unwrap(),
                line_instance: 2,
            },
        )
        .unwrap();
        assert_eq!(
            ClientMessage::decode_with_version(
                Frame::new(
                    ProtocolVersion::V3.wire(),
                    wire_id::ENBLOC_CALL,
                    early_with_line,
                ),
                ProtocolVersion::V3,
            )
            .unwrap(),
            ClientMessage::EnblocCall {
                called_party: "2001".into(),
                line_instance: 2,
            }
        );

        let packed = encode(
            wire_id::ENBLOC_CALL,
            &WireEnblocWithLine::<25, 0> {
                called_party: WireAlignedText::new(wire_id::ENBLOC_CALL, "called party", "2001")
                    .unwrap(),
                line_instance: 2,
            },
        )
        .unwrap();
        assert_eq!(packed.len(), 29);
        assert_eq!(
            ClientMessage::decode_with_version(
                Frame::new(
                    ProtocolVersion::V22.wire(),
                    wire_id::ENBLOC_CALL,
                    packed.clone(),
                ),
                ProtocolVersion::V22,
            )
            .unwrap(),
            ClientMessage::EnblocCall {
                called_party: "2001".into(),
                line_instance: 2,
            }
        );

        let mut padded = encode(
            wire_id::ENBLOC_CALL,
            &WireEnblocWithLine::<25, 0> {
                called_party: WireAlignedText::new(wire_id::ENBLOC_CALL, "called party", "2001")
                    .unwrap(),
                line_instance: 1,
            },
        )
        .unwrap();
        padded.extend_from_slice(&[0; 3]);
        assert_eq!(padded.len(), 32);
        assert_eq!(
            ClientMessage::decode_with_version(
                Frame::new(ProtocolVersion::V22.wire(), wire_id::ENBLOC_CALL, padded,),
                ProtocolVersion::V22,
            )
            .unwrap(),
            ClientMessage::EnblocCall {
                called_party: "2001".into(),
                line_instance: 1,
            }
        );

        let aligned = encode(
            wire_id::ENBLOC_CALL,
            &WireEnblocWithLine::<25, 3> {
                called_party: WireAlignedText::new(wire_id::ENBLOC_CALL, "called party", "2001")
                    .unwrap(),
                line_instance: MAX_STATION_BUTTON_INSTANCE,
            },
        )
        .unwrap();
        assert_eq!(
            ClientMessage::decode_with_version(
                Frame::new(ProtocolVersion::V22.wire(), wire_id::ENBLOC_CALL, aligned),
                ProtocolVersion::V22,
            )
            .unwrap(),
            ClientMessage::EnblocCall {
                called_party: "2001".into(),
                line_instance: MAX_STATION_BUTTON_INSTANCE,
            }
        );

        let invalid_aligned_line = encode(
            wire_id::ENBLOC_CALL,
            &WireEnblocWithLine::<25, 3> {
                called_party: WireAlignedText::new(wire_id::ENBLOC_CALL, "called party", "2001")
                    .unwrap(),
                line_instance: MAX_STATION_BUTTON_INSTANCE + 1,
            },
        )
        .unwrap();
        assert!(
            ClientMessage::decode_with_version(
                Frame::new(
                    ProtocolVersion::V22.wire(),
                    wire_id::ENBLOC_CALL,
                    invalid_aligned_line,
                ),
                ProtocolVersion::V22,
            )
            .is_err()
        );

        let invalid_line = encode(
            wire_id::ENBLOC_CALL,
            &WireEnblocWithLine::<25, 0> {
                called_party: WireAlignedText::new(wire_id::ENBLOC_CALL, "called party", "2001")
                    .unwrap(),
                line_instance: MAX_STATION_BUTTON_INSTANCE + 1,
            },
        )
        .unwrap();
        assert!(matches!(
            ClientMessage::decode_with_version(
                Frame::new(
                    ProtocolVersion::V22.wire(),
                    wire_id::ENBLOC_CALL,
                    invalid_line,
                ),
                ProtocolVersion::V22,
            ),
            Err(CodecError::InvalidValue {
                field: "line instance",
                ..
            })
        ));
        assert!(matches!(
            ClientMessage::EnblocCall {
                called_party: "2001".into(),
                line_instance: MAX_STATION_BUTTON_INSTANCE + 1,
            }
            .encode(ProtocolVersion::V22),
            Err(CodecError::InvalidValue {
                field: "line instance",
                ..
            })
        ));
        assert!(matches!(
            ClientMessage::decode_with_version(
                Frame::new(ProtocolVersion::V18.wire(), wire_id::ENBLOC_CALL, packed,),
                ProtocolVersion::V18,
            ),
            Err(CodecError::InvalidLength(wire_id::ENBLOC_CALL))
        ));
    }

    #[test]
    fn on_hook_accepts_only_fieldless_and_identified_layouts() {
        assert_eq!(
            ClientMessage::decode_with_version(
                Frame::new(ProtocolVersion::V22.wire(), wire_id::ON_HOOK, Vec::new()),
                ProtocolVersion::V22,
            )
            .unwrap(),
            ClientMessage::OnHook {
                line_instance: 0,
                call_reference: 0,
            }
        );

        let identified = encode(
            wire_id::ON_HOOK,
            &WireLineCall {
                line_instance: 2,
                call_reference: 42,
            },
        )
        .unwrap();
        assert_eq!(
            ClientMessage::decode_with_version(
                Frame::new(ProtocolVersion::V22.wire(), wire_id::ON_HOOK, identified),
                ProtocolVersion::V22,
            )
            .unwrap(),
            ClientMessage::OnHook {
                line_instance: 2,
                call_reference: 42,
            }
        );

        for payload_bytes in [1, 2, 3, 4, 5, 6, 7, 9, 12] {
            assert!(matches!(
                ClientMessage::decode_with_version(
                    Frame::new(
                        ProtocolVersion::V22.wire(),
                        wire_id::ON_HOOK,
                        vec![0; payload_bytes],
                    ),
                    ProtocolVersion::V22,
                ),
                Err(CodecError::InvalidLength(wire_id::ON_HOOK))
            ));
        }
    }

    #[test]
    fn call_count_request_preserves_known_length_selected_dialects() {
        for (message, expected_payload_len) in [
            (
                ClientMessage::CallCountRequest(CallCountRequestPayload::Empty),
                0,
            ),
            (
                ClientMessage::CallCountRequest(CallCountRequestPayload::LegacyWord(0x1234_5678)),
                4,
            ),
            (
                ClientMessage::CallCountRequest(CallCountRequestPayload::Extended(
                    [0xa5; CALL_COUNT_REQUEST_EXTENDED_BYTES],
                )),
                CALL_COUNT_REQUEST_EXTENDED_BYTES,
            ),
        ] {
            let bytes = message.encode(ProtocolVersion::V22).unwrap();
            let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);

            assert_eq!(frame.payload.len(), expected_payload_len);
            assert_eq!(ClientMessage::decode(frame).unwrap(), message);
        }
    }

    #[test]
    fn call_count_request_rejects_unknown_payload_length() {
        let frame = Frame {
            protocol_version: ProtocolVersion::V22.wire(),
            message_id: wire_id::CALL_COUNT_REQ,
            payload: vec![0; 8],
        };

        assert!(matches!(
            ClientMessage::decode(frame),
            Err(CodecError::InvalidValue {
                field: "call-count request payload length",
                value: 8,
                ..
            })
        ));
    }

    #[test]
    fn call_count_response_zero_pads_all_forty_two_line_slots() {
        let message = ServerMessage::CallCountResponse(CallCountResponse {
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
        });

        let bytes = message.encode(ProtocolVersion::V22).unwrap();
        let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);

        assert_eq!(frame.payload.len(), 12 + 42 * 4);
        assert_eq!(
            ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
            message
        );
    }

    #[test]
    fn connection_statistics_reject_oversized_quality_payloads() {
        let payload = WireConnectionStatisticsV19 {
            directory_number: WireAlignedText::new(
                wire_id::CONNECTION_STATISTICS_RES,
                "directory number",
                "2002",
            )
            .unwrap(),
            call_reference: 42,
            processing: StatisticsProcessing::Clear.wire_value(),
            statistics: WireConnectionStatisticsTail {
                counters: WireConnectionStatisticsCounters {
                    packets_sent: 1,
                    octets_sent: 2,
                    packets_received: 3,
                    octets_received: 4,
                    packets_lost: 5,
                    jitter_millis: 6,
                    latency_millis: 7,
                },
                quality_size: (CONNECTION_QUALITY_MAX_BYTES + 1) as u32,
            },
            quality: vec![0; CONNECTION_QUALITY_MAX_BYTES + 1],
        };
        let mut encoded = encode(wire_id::CONNECTION_STATISTICS_RES, &payload).unwrap();
        pad_dynamic_payload(&mut encoded);
        let frame = Frame::new(
            ProtocolVersion::V22.wire(),
            wire_id::CONNECTION_STATISTICS_RES,
            encoded,
        );
        assert!(matches!(
            ClientMessage::decode_with_version(frame, ProtocolVersion::V22),
            Err(CodecError::CountTooLarge {
                field: "quality statistics",
                maximum: CONNECTION_QUALITY_MAX_BYTES,
                ..
            })
        ));
    }

    #[test]
    fn packed_connection_statistics_accept_zero_padding_without_a_quality_size() {
        let mut payload = encode(
            wire_id::CONNECTION_STATISTICS_RES,
            &WireConnectionStatisticsPackedBase {
                directory_number: WireAlignedText::new(
                    wire_id::CONNECTION_STATISTICS_RES,
                    "directory number",
                    "2002",
                )
                .unwrap(),
                call_reference: 0x1122_3344,
                processing: StatisticsProcessing::DoNotClear.wire_value() as u8,
                counters: WireConnectionStatisticsCounters {
                    packets_sent: 0x0102_0304,
                    octets_sent: 0x1112_1314,
                    packets_received: 0x2122_2324,
                    octets_received: 0x3132_3334,
                    packets_lost: 0x4142_4344,
                    jitter_millis: 0x5152_5354,
                    latency_millis: 0x6162_6364,
                },
            },
        )
        .unwrap();
        assert_eq!(payload.len(), 61);
        payload.extend_from_slice(&[0; 3]);

        let message = ClientMessage::decode_with_version(
            Frame::new(
                ProtocolVersion::V22.wire(),
                wire_id::CONNECTION_STATISTICS_RES,
                payload,
            ),
            ProtocolVersion::V22,
        )
        .unwrap();
        assert_eq!(
            message,
            ClientMessage::ConnectionStatisticsResponse(ConnectionStatistics {
                directory_number: "2002".into(),
                call_reference: 0x1122_3344,
                processing: StatisticsProcessing::DoNotClear,
                packets_sent: 0x0102_0304,
                octets_sent: 0x1112_1314,
                packets_received: 0x2122_2324,
                octets_received: 0x3132_3334,
                packets_lost: 0x4142_4344,
                jitter_millis: 0x5152_5354,
                latency_millis: 0x6162_6364,
                quality: ConnectionQualityStatistics::new(Vec::new()).unwrap(),
            })
        );
    }

    #[test]
    fn packed_connection_statistics_accepts_a_non_word_aligned_declared_tail() {
        let quality = (0_u8..110).collect::<Vec<_>>();
        let mut payload = encode(
            wire_id::CONNECTION_STATISTICS_RES,
            &WireConnectionStatisticsPackedPrefix {
                base: WireConnectionStatisticsPackedBase {
                    directory_number: WireAlignedText::new(
                        wire_id::CONNECTION_STATISTICS_RES,
                        "directory number",
                        "2002",
                    )
                    .unwrap(),
                    call_reference: 42,
                    processing: StatisticsProcessing::Clear.wire_value() as u8,
                    counters: WireConnectionStatisticsCounters {
                        packets_sent: 1,
                        octets_sent: 2,
                        packets_received: 3,
                        octets_received: 4,
                        packets_lost: 5,
                        jitter_millis: 6,
                        latency_millis: 7,
                    },
                },
                quality_size: quality.len() as u32,
            },
        )
        .unwrap();
        payload.extend_from_slice(&quality);
        assert_eq!(payload.len(), 175);

        let decoded = ClientMessage::decode_with_version(
            Frame::new(
                ProtocolVersion::V22.wire(),
                wire_id::CONNECTION_STATISTICS_RES,
                payload.clone(),
            ),
            ProtocolVersion::V22,
        )
        .unwrap();
        let ClientMessage::ConnectionStatisticsResponse(statistics) = decoded else {
            panic!("expected connection statistics response");
        };
        assert_eq!(statistics.directory_number, "2002");
        assert_eq!(statistics.call_reference, 42);
        assert_eq!(statistics.quality.as_bytes(), quality);

        payload.extend_from_slice(&[0; 3]);
        assert!(
            ClientMessage::decode_with_version(
                Frame::new(
                    ProtocolVersion::V22.wire(),
                    wire_id::CONNECTION_STATISTICS_RES,
                    payload.clone(),
                ),
                ProtocolVersion::V22,
            )
            .is_ok()
        );
        *payload.last_mut().unwrap() = 1;
        assert!(
            ClientMessage::decode_with_version(
                Frame::new(
                    ProtocolVersion::V22.wire(),
                    wire_id::CONNECTION_STATISTICS_RES,
                    payload,
                ),
                ProtocolVersion::V22,
            )
            .is_err()
        );
    }

    #[test]
    fn aligned_connection_statistics_discards_inactive_fixed_reservoir_storage() {
        let quality = (0_u8..113).collect::<Vec<_>>();
        let mut payload = encode(
            wire_id::CONNECTION_STATISTICS_RES,
            &WireConnectionStatisticsV19Prefix {
                directory_number: WireAlignedText::new(
                    wire_id::CONNECTION_STATISTICS_RES,
                    "directory number",
                    "2002",
                )
                .unwrap(),
                call_reference: 42,
                processing: StatisticsProcessing::Clear.wire_value(),
                statistics: WireConnectionStatisticsTail {
                    counters: WireConnectionStatisticsCounters {
                        packets_sent: 1,
                        octets_sent: 2,
                        packets_received: 3,
                        octets_received: 4,
                        packets_lost: 5,
                        jitter_millis: 6,
                        latency_millis: 7,
                    },
                    quality_size: quality.len() as u32,
                },
            },
        )
        .unwrap();
        payload.extend_from_slice(&quality);
        payload.extend_from_slice(&[0; CONNECTION_QUALITY_MAX_BYTES - 113]);
        assert_eq!(payload.len(), 668);
        assert_eq!(payload.len() - 68 - quality.len(), 487);

        let decoded = ClientMessage::decode_with_version(
            Frame::new(
                ProtocolVersion::V22.wire(),
                wire_id::CONNECTION_STATISTICS_RES,
                payload.clone(),
            ),
            ProtocolVersion::V22,
        )
        .unwrap();
        let ClientMessage::ConnectionStatisticsResponse(statistics) = decoded else {
            panic!("expected connection statistics response");
        };
        assert_eq!(statistics.quality.as_bytes(), quality);

        payload[68 + quality.len()..].fill(0xa5);
        let decoded = ClientMessage::decode_with_version(
            Frame::new(
                ProtocolVersion::V22.wire(),
                wire_id::CONNECTION_STATISTICS_RES,
                payload,
            ),
            ProtocolVersion::V22,
        )
        .unwrap();
        let ClientMessage::ConnectionStatisticsResponse(statistics) = decoded else {
            panic!("expected connection statistics response");
        };
        assert_eq!(statistics.quality.as_bytes(), quality);
    }

    #[test]
    fn protocol_19_connection_statistics_accepts_each_transition_shape() {
        let message = ClientMessage::ConnectionStatisticsResponse(ConnectionStatistics {
            directory_number: "2002".into(),
            call_reference: 42,
            processing: StatisticsProcessing::Clear,
            packets_sent: 1,
            octets_sent: 2,
            packets_received: 3,
            octets_received: 4,
            packets_lost: 5,
            jitter_millis: 6,
            latency_millis: 8,
            quality: ConnectionQualityStatistics::new(vec![0xa1, 0xb2, 0xc3]).unwrap(),
        });

        for encoded_as in [ProtocolVersion::V18, ProtocolVersion::V19] {
            let frame = FrameDecoder::new()
                .push(&message.encode(encoded_as).unwrap())
                .unwrap()
                .remove(0);
            assert_eq!(
                ClientMessage::decode_with_version(
                    Frame::new(
                        ProtocolVersion::V19.wire(),
                        wire_id::CONNECTION_STATISTICS_RES,
                        frame.payload,
                    ),
                    ProtocolVersion::V19,
                )
                .unwrap(),
                message,
                "encoded with protocol {}",
                encoded_as.wire(),
            );
        }
    }

    #[test]
    fn canonical_connection_statistics_from_protocol_19_use_the_aligned_layout() {
        let quality = vec![0xa1, 0xb2, 0xc3];
        let message = ClientMessage::ConnectionStatisticsResponse(ConnectionStatistics {
            directory_number: "2002".into(),
            call_reference: 0x1122_3344,
            processing: StatisticsProcessing::DoNotClear,
            packets_sent: 0x0102_0304,
            octets_sent: 0x1112_1314,
            packets_received: 0x2122_2324,
            octets_received: 0x3132_3334,
            packets_lost: 0x4142_4344,
            jitter_millis: 0x5152_5354,
            latency_millis: 0x6162_6364,
            quality: ConnectionQualityStatistics::new(quality.clone()).unwrap(),
        });
        let encoded = message.encode(ProtocolVersion::V22).unwrap();
        let frame = FrameDecoder::new().push(&encoded).unwrap().remove(0);

        assert_eq!(&frame.payload[28..32], &0x1122_3344_u32.to_le_bytes());
        assert_eq!(&frame.payload[32..36], &1_u32.to_le_bytes());
        assert_eq!(&frame.payload[36..40], &0x0102_0304_u32.to_le_bytes());
        assert_eq!(&frame.payload[40..44], &0x1112_1314_u32.to_le_bytes());
        assert_eq!(&frame.payload[44..48], &0x2122_2324_u32.to_le_bytes());
        assert_eq!(&frame.payload[48..52], &0x3132_3334_u32.to_le_bytes());
        assert_eq!(&frame.payload[52..56], &0x4142_4344_u32.to_le_bytes());
        assert_eq!(&frame.payload[56..60], &0x5152_5354_u32.to_le_bytes());
        assert_eq!(&frame.payload[60..64], &0x6162_6364_u32.to_le_bytes());
        assert_eq!(&frame.payload[64..68], &3_u32.to_le_bytes());
        assert_eq!(&frame.payload[68..71], quality.as_slice());
        assert_eq!(
            ClientMessage::decode_with_version(frame, ProtocolVersion::V22).unwrap(),
            message
        );
    }

    #[test]
    fn media_wire_schemas_round_trip_byte_for_byte() {
        let start = fixture(include_str!(
            "../../tests/fixtures/golden/start_media_transmission_v17.hex"
        ));
        let start_value: WireStartMediaV17 = decode(0x008a, &start[12..]).unwrap();
        assert_eq!(encode(0x008a, &start_value).unwrap(), &start[12..]);
        let start_frame = FrameDecoder::new().push(&start).unwrap().remove(0);
        let start_message = ServerMessage::decode(start_frame, ProtocolVersion::V17).unwrap();
        assert_eq!(start_message.encode(ProtocolVersion::V17).unwrap(), start);

        let open = fixture(include_str!(
            "../../tests/fixtures/golden/open_receive_channel_v17.hex"
        ));
        let open_value: WireOpenReceiveV17 = decode(0x0105, &open[12..]).unwrap();
        assert_eq!(encode(0x0105, &open_value).unwrap(), &open[12..]);
        let open_frame = FrameDecoder::new().push(&open).unwrap().remove(0);
        let open_message = ServerMessage::decode(open_frame, ProtocolVersion::V17).unwrap();
        assert_eq!(open_message.encode(ProtocolVersion::V17).unwrap(), open);

        let ack = fixture(include_str!(
            "../../tests/fixtures/golden/start_media_transmission_ack_v20.hex"
        ));
        let ack_value: WireStartMediaAckV20 = decode(0x0154, &ack[12..]).unwrap();
        assert_eq!(encode(0x0154, &ack_value).unwrap(), &ack[12..]);
        let ack_frame = FrameDecoder::new().push(&ack).unwrap().remove(0);
        let ack_message =
            ClientMessage::decode_with_version(ack_frame, ProtocolVersion::V20).unwrap();
        assert_eq!(ack_message.encode(ProtocolVersion::V20).unwrap(), ack);
    }

    #[test]
    fn audio_media_version_boundaries_have_exact_payload_sizes() {
        let endpoint = MediaEndpoint {
            address: "192.0.2.20".parse().unwrap(),
            rtp_port: 16_000,
            rtcp_port: 16_001,
            codec: Codec::Pcmu,
            packet_ms: 20,
            max_frames_per_packet: 2,
            telephone_event_payload: 101,
        };
        for (version, open_size, start_size) in [
            (11, 92, 108),
            (12, 108, 116),
            (16, 108, 116),
            (17, 128, 132),
            (18, 132, 132),
            (20, 132, 132),
            (21, 168, 168),
            (22, 168, 168),
        ] {
            let protocol = ProtocolVersion::new(version).unwrap();
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
            };
            let open_bytes = open.encode(protocol).unwrap();
            assert_eq!(open_bytes.len() - 12, open_size, "protocol {version}");
            let decoded = ServerMessage::decode(
                FrameDecoder::new().push(&open_bytes).unwrap().remove(0),
                protocol,
            )
            .unwrap();
            assert_eq!(decoded.encode(protocol).unwrap(), open_bytes);

            let start = ServerMessage::StartMediaTransmission {
                call_reference: 7,
                passthrough_party_id: 9,
                endpoint,
                silence_suppression: SilenceSuppression::Off,
                traffic_class: MediaTrafficClass::default(),
                encryption: None,
                wire: None,
            };
            let start_bytes = start.encode(protocol).unwrap();
            assert_eq!(start_bytes.len() - 12, start_size, "protocol {version}");
            let decoded = ServerMessage::decode(
                FrameDecoder::new().push(&start_bytes).unwrap().remove(0),
                protocol,
            )
            .unwrap();
            assert_eq!(decoded.encode(protocol).unwrap(), start_bytes);
        }
    }

    #[test]
    fn audio_acknowledgements_keep_conference_and_call_references_distinct() {
        for (protocol, address, open_size, start_size, failure_size) in [
            (
                ProtocolVersion::V16,
                "192.0.2.21".parse().unwrap(),
                20,
                24,
                20,
            ),
            (
                ProtocolVersion::V17,
                "2001:db8::21".parse().unwrap(),
                36,
                40,
                36,
            ),
        ] {
            let open = ClientMessage::OpenReceiveChannelAck {
                status: MediaStatus::Ok,
                address,
                port: 16_000,
                passthrough_party_id: 9,
                call_reference: 7,
            };
            let open_bytes = open.encode(protocol).unwrap();
            assert_eq!(open_bytes.len() - 12, open_size);
            assert_eq!(
                ClientMessage::decode_with_version(
                    FrameDecoder::new().push(&open_bytes).unwrap().remove(0),
                    protocol,
                )
                .unwrap(),
                open
            );

            let start = ClientMessage::StartMediaTransmissionAck(MediaTransmissionAck {
                conference_id: 6,
                passthrough_party_id: 9,
                call_reference: 7,
                status: MediaStatus::Ok,
                address,
                port: 16_000,
                wire: None,
            });
            let start_bytes = start.encode(protocol).unwrap();
            assert_eq!(start_bytes.len() - 12, start_size);
            let decoded = ClientMessage::decode_with_version(
                FrameDecoder::new().push(&start_bytes).unwrap().remove(0),
                protocol,
            )
            .unwrap();
            assert_eq!(decoded, start);

            let failure = ClientMessage::MediaTransmissionFailure {
                conference_id: 6,
                passthrough_party_id: 9,
                address,
                port: 16_000,
                call_reference: 7,
                status: MediaStatus::UnspecifiedError,
            };
            let failure_bytes = failure.encode(protocol).unwrap();
            assert_eq!(failure_bytes.len() - 12, failure_size);
            assert_eq!(
                ClientMessage::decode_with_version(
                    FrameDecoder::new().push(&failure_bytes).unwrap().remove(0),
                    protocol,
                )
                .unwrap(),
                failure
            );
        }

        for message_id in [
            wire_id::CLOSE_RECEIVE_CHANNEL,
            wire_id::STOP_MEDIA_TRANSMISSION,
        ] {
            let payload = [0_u8; 16];
            let frame = Frame::new(ProtocolVersion::V22.wire(), message_id, payload.to_vec());
            let message = ServerMessage::decode(frame, ProtocolVersion::V22).unwrap();
            assert_eq!(message.encode(ProtocolVersion::V22).unwrap().len() - 12, 16);

            let truncated = Frame::new(
                ProtocolVersion::V22.wire(),
                message_id,
                payload[..12].to_vec(),
            );
            assert!(matches!(
                ServerMessage::decode(truncated, ProtocolVersion::V22),
                Err(CodecError::Truncated { .. })
            ));
        }
    }

    #[test]
    fn session_and_video_envelopes_preserve_every_wire_byte() {
        for (protocol, address, expected_size) in [
            (ProtocolVersion::V16, "192.0.2.30".parse().unwrap(), 8),
            (ProtocolVersion::V17, "2001:db8::30".parse().unwrap(), 24),
        ] {
            for message in [
                ControlMessage::StartSessionTransmission(SessionTransmission {
                    remote_address: address,
                    session_type: 0x1122_3344,
                }),
                ControlMessage::StopSessionTransmission(SessionTransmission {
                    remote_address: address,
                    session_type: 0x5566_7788,
                }),
            ] {
                let bytes = message.encode(protocol).unwrap();
                assert_eq!(bytes.len() - 12, expected_size);
                let decoded = ControlMessage::decode(
                    FrameDecoder::new().push(&bytes).unwrap().remove(0),
                    protocol,
                )
                .unwrap();
                assert_eq!(decoded.encode(protocol).unwrap(), bytes);
            }
        }

        for (version, address, expected_size) in [
            (11, "0.0.0.0".parse().unwrap(), 164),
            (12, "192.0.2.31".parse().unwrap(), 172),
            (16, "192.0.2.31".parse().unwrap(), 172),
            (17, "2001:db8::31".parse().unwrap(), 192),
        ] {
            let protocol = ProtocolVersion::new(version).unwrap();
            let message = ServerMessage::OpenMultimediaChannel(OpenMultimediaChannel {
                conference_id: 42.into(),
                passthrough_party_id: 9.into(),
                line_instance: 1,
                call_reference: 7.into(),
                payload: typed_video_payload(MultimediaVideoCapabilityArm::H264 {
                    profile: 100,
                    level: 42,
                    custom_max_mbps: 40_500,
                    custom_max_fs: 1_620,
                    custom_max_dpb: 8_100,
                    custom_max_br_and_cpb: 10_000,
                }),
                conference_creator: true,
                encryption: None,
                stream_passthrough_id: 10,
                associated_stream_id: 11,
                source: MediaEndpointAddress {
                    address,
                    port: if version < 12 { 0 } else { 16_000 },
                },
                requested_address_type: if version >= 17 {
                    IpAddressType::Ipv6
                } else {
                    IpAddressType::Ipv4
                },
            });
            let bytes = message.encode(protocol).unwrap();
            assert_eq!(bytes.len() - 12, expected_size, "protocol {version}");
            let decoded = ServerMessage::decode(
                FrameDecoder::new().push(&bytes).unwrap().remove(0),
                protocol,
            )
            .unwrap();
            assert_eq!(decoded.encode(protocol).unwrap(), bytes);
        }

        for (protocol, address, expected_size) in [
            (ProtocolVersion::V16, "192.0.2.32".parse().unwrap(), 168),
            (ProtocolVersion::V17, "2001:db8::32".parse().unwrap(), 184),
        ] {
            let message = ServerMessage::StartMultimediaTransmission(StartMultimediaTransmission {
                conference_id: 42.into(),
                passthrough_party_id: 9.into(),
                endpoint: MediaEndpointAddress {
                    address,
                    port: 16_002,
                },
                call_reference: 7.into(),
                payload: typed_video_payload(MultimediaVideoCapabilityArm::H264 {
                    profile: 100,
                    level: 42,
                    custom_max_mbps: 40_500,
                    custom_max_fs: 1_620,
                    custom_max_dpb: 8_100,
                    custom_max_br_and_cpb: 10_000,
                }),
                traffic_class: MediaTrafficClass::from_wire(184),
                encryption: None,
                stream_passthrough_id: 10,
                associated_stream_id: 11,
            });
            let bytes = message.encode(protocol).unwrap();
            assert_eq!(bytes.len() - 12, expected_size);
            let traffic_class_offset = match protocol.wire() {
                17.. => 60,
                _ => 44,
            };
            assert_eq!(
                &bytes[traffic_class_offset..traffic_class_offset + 4],
                &184_u32.to_le_bytes()
            );
            let decoded = ServerMessage::decode(
                FrameDecoder::new().push(&bytes).unwrap().remove(0),
                protocol,
            )
            .unwrap();
            assert_eq!(decoded.encode(protocol).unwrap(), bytes);
        }

        let miscellaneous = ServerMessage::MiscellaneousCommand(MiscellaneousCommand {
            conference_id: 42.into(),
            passthrough_party_id: 9.into(),
            call_reference: 7.into(),
            command: values::MiscCommandType::LostPartialPicture,
            data: BoundedBytes::try_from((0_u8..36).collect::<Vec<_>>()).unwrap(),
        });
        let bytes = miscellaneous.encode(ProtocolVersion::V22).unwrap();
        assert_eq!(bytes.len() - 12, 52);
        let decoded = ServerMessage::decode(
            FrameDecoder::new().push(&bytes).unwrap().remove(0),
            ProtocolVersion::V22,
        )
        .unwrap();
        assert_eq!(decoded, miscellaneous);

        for (message_id, protocol, expected) in [
            (wire_id::OPEN_MULTIMEDIA_CHANNEL, ProtocolVersion::V17, 192),
            (
                wire_id::START_MULTIMEDIA_TRANSMISSION,
                ProtocolVersion::V17,
                184,
            ),
            (wire_id::MISCELLANEOUS_COMMAND, ProtocolVersion::V22, 52),
        ] {
            for actual in [expected - 1, expected + 1] {
                let frame = Frame::new(protocol.wire(), message_id, vec![0; actual]);
                assert!(ServerMessage::decode(frame, protocol).is_err());
            }
        }
        for (protocol, expected) in [(ProtocolVersion::V16, 8), (ProtocolVersion::V17, 24)] {
            for actual in [expected - 1, expected + 1] {
                let frame = Frame::new(
                    protocol.wire(),
                    wire_id::START_SESSION_TRANSMISSION,
                    vec![0; actual],
                );
                assert!(ControlMessage::decode(frame, protocol).is_err());
            }
        }
    }

    #[test]
    fn unsupported_decoded_multimedia_payloads_are_lossless_and_provenance_bound() {
        let mut words = [0; MULTIMEDIA_CAPABILITY_BYTES / 4];
        words[0] = 2_048;
        words[1] = 1;
        words[2] = VideoFormat::Cif.wire_value();
        words[3] = 2;
        words[12] = 7;
        words[13..].copy_from_slice(&[61, 62, 63, 64, 65, 66]);
        let capability = multimedia_capability_bytes(words);
        let payload = MultimediaPayload::from_wire(
            0,
            test_rtp_payload_number(97),
            capability,
            Codec::H265,
            MultimediaPayloadDirection::Receive,
            ProtocolVersion::V17,
        );
        assert_eq!(payload.codec(), Codec::H265);
        assert_eq!(payload.video_capability(), None);
        let open = ServerMessage::OpenMultimediaChannel(OpenMultimediaChannel {
            conference_id: 42.into(),
            passthrough_party_id: 9.into(),
            line_instance: 1,
            call_reference: 7.into(),
            payload: payload.clone(),
            conference_creator: false,
            encryption: None,
            stream_passthrough_id: 10,
            associated_stream_id: 0,
            source: MediaEndpointAddress {
                address: "192.0.2.31".parse().unwrap(),
                port: 16_000,
            },
            requested_address_type: IpAddressType::Ipv4,
        });
        let encoded = open.encode(ProtocolVersion::V17).unwrap();
        let decoded = ServerMessage::decode(
            FrameDecoder::new().push(&encoded).unwrap().remove(0),
            ProtocolVersion::V17,
        )
        .unwrap();
        assert_eq!(decoded.encode(ProtocolVersion::V17).unwrap(), encoded);
        assert!(matches!(
            open.encode(ProtocolVersion::V16),
            Err(CodecError::InvalidValue {
                message_id: wire_id::OPEN_MULTIMEDIA_CHANNEL,
                field: "multimedia payload provenance",
                ..
            })
        ));

        let start = ServerMessage::StartMultimediaTransmission(StartMultimediaTransmission {
            conference_id: 42.into(),
            passthrough_party_id: 9.into(),
            endpoint: MediaEndpointAddress {
                address: "192.0.2.32".parse().unwrap(),
                port: 16_002,
            },
            call_reference: 7.into(),
            payload,
            traffic_class: MediaTrafficClass::from_wire(136),
            encryption: None,
            stream_passthrough_id: 11,
            associated_stream_id: 0,
        });
        assert!(matches!(
            start.encode(ProtocolVersion::V17),
            Err(CodecError::InvalidValue {
                message_id: wire_id::START_MULTIMEDIA_TRANSMISSION,
                field: "multimedia payload provenance",
                ..
            })
        ));
    }

    #[test]
    fn capabilities_incompatible_with_outer_compression_remain_opaque_and_lossless() {
        let message = ServerMessage::OpenMultimediaChannel(OpenMultimediaChannel {
            conference_id: 42.into(),
            passthrough_party_id: 9.into(),
            line_instance: 1,
            call_reference: 7.into(),
            payload: typed_video_payload(MultimediaVideoCapabilityArm::H264 {
                profile: 100,
                level: 42,
                custom_max_mbps: 40_500,
                custom_max_fs: 1_620,
                custom_max_dpb: 8_100,
                custom_max_br_and_cpb: 10_000,
            }),
            conference_creator: false,
            encryption: None,
            stream_passthrough_id: 10,
            associated_stream_id: 0,
            source: MediaEndpointAddress {
                address: "192.0.2.31".parse().unwrap(),
                port: 16_000,
            },
            requested_address_type: IpAddressType::Ipv4,
        });
        let mut mismatched = message.encode(ProtocolVersion::V17).unwrap();
        let compression_offset = super::wire::HEADER_SIZE + 8;
        mismatched[compression_offset..compression_offset + 4]
            .copy_from_slice(&Codec::H263.wire_value().to_le_bytes());

        let decoded = ServerMessage::decode(
            FrameDecoder::new().push(&mismatched).unwrap().remove(0),
            ProtocolVersion::V17,
        )
        .unwrap();
        let ServerMessage::OpenMultimediaChannel(open) = &decoded else {
            panic!("multimedia message decoded as a different command");
        };
        assert_eq!(open.payload.codec(), Codec::H263);
        assert_eq!(open.payload.compression_codec(), Codec::H263);
        assert_eq!(open.payload.video_capability(), None);
        assert_eq!(decoded.encode(ProtocolVersion::V17).unwrap(), mismatched);
        assert_ne!(decoded, message);

        let mut invalid_payload_number = mismatched;
        let descriptor_offset = super::wire::HEADER_SIZE + 20;
        invalid_payload_number[descriptor_offset + 4..descriptor_offset + 8]
            .copy_from_slice(&128_u32.to_le_bytes());
        assert!(matches!(
            ServerMessage::decode(
                FrameDecoder::new()
                    .push(&invalid_payload_number)
                    .unwrap()
                    .remove(0),
                ProtocolVersion::V17,
            ),
            Err(CodecError::InvalidValue {
                field: "RTP payload number",
                value: 128,
                ..
            })
        ));
    }

    #[test]
    fn typed_multimedia_video_arms_encode_at_the_evidenced_offsets() {
        let arms = [
            (
                MultimediaVideoCapabilityArm::H261 {
                    temporal_spatial_trade_off_capability: 11,
                    still_image_transmission: 12,
                },
                [11, 12, 0, 0, 0, 0],
            ),
            (
                MultimediaVideoCapabilityArm::H263 {
                    capability_bitfield: 21,
                    annex_n_and_w_future_use: 22,
                },
                [21, 22, 0, 0, 0, 0],
            ),
            (
                MultimediaVideoCapabilityArm::H263Plus {
                    model_number: 31,
                    bandwidth: 32,
                },
                [31, 32, 0, 0, 0, 0],
            ),
            (
                MultimediaVideoCapabilityArm::H264 {
                    profile: 41,
                    level: 42,
                    custom_max_mbps: 43,
                    custom_max_fs: 44,
                    custom_max_dpb: 45,
                    custom_max_br_and_cpb: 46,
                },
                [41, 42, 43, 44, 45, 46],
            ),
        ];

        for (arm, expected_arm) in arms {
            let payload = typed_video_payload(arm);
            let bytes = multimedia_capability_to_wire(&payload);
            let words = multimedia_capability_words(bytes);
            assert_eq!(words[0], 1_024);
            assert_eq!(words[1], 2);
            assert_eq!(
                &words[2..6],
                &[
                    VideoFormat::Cif4.wire_value(),
                    1,
                    VideoFormat::Cif.wire_value(),
                    2,
                ]
            );
            assert_eq!(&words[6..12], &[0; 6]);
            assert_eq!(words[12], 7);
            assert_eq!(&words[13..], &expected_arm);

            let decoded = decoded_multimedia_capability(bytes, arm.codec());
            let MultimediaCapabilityState::Video(decoded) = decoded else {
                panic!("typed codec arm was not decoded");
            };
            assert_eq!(decoded.arm(), arm);
            assert_eq!(decoded.picture_formats().len(), 2);
            let decoded_payload = MultimediaPayload::from_decoded(
                payload.descriptor(),
                MultimediaCapabilityState::Video(decoded),
                MultimediaPayloadDirection::Transmit,
                ProtocolVersion::V17,
                arm.codec(),
            );
            assert_eq!(multimedia_capability_to_wire(&decoded_payload), bytes);
        }
    }

    #[test]
    fn multimedia_descriptor_carries_the_negotiated_rtp_mapping() {
        for (arm, expected_payload_number) in [
            (
                MultimediaVideoCapabilityArm::H261 {
                    temporal_spatial_trade_off_capability: 0,
                    still_image_transmission: 0,
                },
                31,
            ),
            (
                MultimediaVideoCapabilityArm::H263 {
                    capability_bitfield: 0,
                    annex_n_and_w_future_use: 0,
                },
                34,
            ),
            (
                MultimediaVideoCapabilityArm::H263Plus {
                    model_number: 0,
                    bandwidth: 0,
                },
                96,
            ),
            (
                MultimediaVideoCapabilityArm::H264 {
                    profile: 0,
                    level: 0,
                    custom_max_mbps: 0,
                    custom_max_fs: 0,
                    custom_max_dpb: 0,
                    custom_max_br_and_cpb: 0,
                },
                97,
            ),
        ] {
            let payload = typed_video_payload(arm);
            let descriptor = payload.descriptor();
            assert_eq!(descriptor.rfc_number(), 0);
            assert_eq!(descriptor.payload_number().get(), expected_payload_number);
            assert_eq!(payload.codec(), arm.codec());
            assert_eq!(
                encode(
                    wire_id::OPEN_MULTIMEDIA_CHANNEL,
                    &WireMultimediaPayloadDescriptor::from(descriptor)
                )
                .unwrap(),
                [0, 0, 0, 0, expected_payload_number, 0, 0, 0,]
            );
        }

        let payload = typed_video_payload(MultimediaVideoCapabilityArm::H263 {
            capability_bitfield: 0,
            annex_n_and_w_future_use: 0,
        });
        let descriptor = MultimediaPayloadDescriptor::new(4, payload.payload_number());
        assert_eq!(
            encode(
                wire_id::OPEN_MULTIMEDIA_CHANNEL,
                &WireMultimediaPayloadDescriptor::from(descriptor),
            )
            .unwrap(),
            [4, 0, 0, 0, 34, 0, 0, 0]
        );
    }

    #[test]
    fn multimedia_acknowledgements_use_distinct_versioned_layouts() {
        for (protocol, address, open_size, start_size) in [
            (ProtocolVersion::V16, "192.0.2.33".parse().unwrap(), 20, 24),
            (
                ProtocolVersion::V17,
                "2001:db8::33".parse().unwrap(),
                36,
                40,
            ),
        ] {
            let open =
                ClientMessage::OpenMultimediaReceiveChannelAck(OpenMultimediaReceiveChannelAck {
                    status: MediaStatus::Ok,
                    endpoint: MediaEndpointAddress {
                        address,
                        port: 16_000,
                    },
                    passthrough_party_id: 9.into(),
                    call_reference: 7.into(),
                });
            let open_bytes = open.encode(protocol).unwrap();
            assert_eq!(open_bytes.len() - 12, open_size);
            assert_eq!(
                ClientMessage::decode_with_version(
                    FrameDecoder::new().push(&open_bytes).unwrap().remove(0),
                    protocol,
                )
                .unwrap(),
                open
            );

            let start =
                ClientMessage::StartMultimediaTransmissionAck(StartMultimediaTransmissionAck {
                    conference_id: 42.into(),
                    passthrough_party_id: 9.into(),
                    call_reference: 7.into(),
                    endpoint: MediaEndpointAddress {
                        address,
                        port: 16_002,
                    },
                    status: MediaStatus::Ok,
                });
            let start_bytes = start.encode(protocol).unwrap();
            assert_eq!(start_bytes.len() - 12, start_size);
            assert_eq!(
                ClientMessage::decode_with_version(
                    FrameDecoder::new().push(&start_bytes).unwrap().remove(0),
                    protocol,
                )
                .unwrap(),
                start
            );
        }
    }

    #[test]
    fn port_messages_switch_layouts_at_protocol_twenty() {
        for (protocol, request_size, close_size, response_size) in [
            (ProtocolVersion::V19, 16, 12, 24),
            (ProtocolVersion::V20, 24, 16, 44),
        ] {
            let extended = protocol.wire() >= 20;
            let request = ServerMessage::PortRequest(PortRequest {
                conference_id: 42.into(),
                call_reference: 7.into(),
                passthrough_party_id: 9.into(),
                transport: MediaTransport::Rtp,
                address_type: extended.then_some(IpAddressType::Ipv4AndIpv6),
                media_type: extended.then_some(MediaType::Audio),
            });
            let request_bytes = request.encode(protocol).unwrap();
            assert_eq!(request_bytes.len() - 12, request_size);
            assert_eq!(
                ServerMessage::decode(
                    FrameDecoder::new().push(&request_bytes).unwrap().remove(0),
                    protocol,
                )
                .unwrap(),
                request
            );

            let close = ServerMessage::PortClose(PortClose {
                conference_id: 42.into(),
                call_reference: 7.into(),
                passthrough_party_id: 9.into(),
                media_type: extended.then_some(MediaType::Audio),
            });
            let close_bytes = close.encode(protocol).unwrap();
            assert_eq!(close_bytes.len() - 12, close_size);
            assert_eq!(
                ServerMessage::decode(
                    FrameDecoder::new().push(&close_bytes).unwrap().remove(0),
                    protocol,
                )
                .unwrap(),
                close
            );

            let response = ControlMessage::PortResponse(PortEndpoint {
                conference_id: 42,
                call_reference: 7,
                passthrough_party_id: 9,
                address: if extended {
                    "2001:db8::34".parse().unwrap()
                } else {
                    "192.0.2.34".parse().unwrap()
                },
                rtp_port: 16_000,
                rtcp_port: 16_001,
                media_type: extended.then_some(MediaType::Audio),
            });
            let response_bytes = response.encode(protocol).unwrap();
            assert_eq!(response_bytes.len() - 12, response_size);
            assert_eq!(
                ControlMessage::decode(
                    FrameDecoder::new().push(&response_bytes).unwrap().remove(0),
                    protocol,
                )
                .unwrap(),
                response
            );
        }
    }

    #[test]
    fn wire_encryption_rejects_invalid_lengths_without_debugging_secrets() {
        let encryption = WireEncryptionInfo {
            algorithm: EncryptionMethod::Aes128HmacSha1_80.wire_value(),
            key_length: 17,
            salt_length: 16,
            key: [0xa5; 16],
            salt: [0x5a; 16],
            mki_present: 1,
            key_derivation_rate: 64,
        };
        let debug = format!("{encryption:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("165"));
        assert!(!debug.contains("90"));

        let error = encryption
            .to_public(wire_id::OPEN_MULTIMEDIA_CHANNEL)
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
    }

    #[test]
    fn wire_encryption_preserves_bytes_after_declared_lengths() {
        let wire = WireEncryptionInfo {
            algorithm: EncryptionMethod::Aes128HmacSha1_80.wire_value(),
            key_length: 1,
            salt_length: 1,
            key: [0xa5, 0x7f, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            salt: [0x5a, 0x6f, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            mki_present: 0,
            key_derivation_rate: 0,
        };
        let encryption = wire.to_public(wire_id::OPEN_MULTIMEDIA_CHANNEL).unwrap();

        assert_eq!(WireEncryptionInfo::from_public(encryption.as_ref()), wire);
        assert_eq!(encryption.as_ref().unwrap().key(), &[0xa5]);
        assert_eq!(encryption.as_ref().unwrap().salt(), &[0x5a]);
    }

    #[test]
    fn announcement_messages_use_bounded_fixed_wire_layouts() {
        let message = ControlMessage::StartAnnouncement {
            announcements: vec![AnnouncementEntry {
                locale: 1,
                country: 46,
                tone: Tone::Zip,
            }],
            end_of_ack: EndOfAnnouncementAck::Required,
            conference_id: 42,
            matrix_conference_party_ids: vec![7, 9],
            hearing_conference_party_mask: 0b11,
            play_mode: AnnouncementPlayMode::Continuous,
        };
        let bytes = message.encode(ProtocolVersion::V22).unwrap();
        let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
        assert_eq!(frame.message_id, wire_id::START_ANNOUNCEMENT);
        assert_eq!(frame.payload.len(), 464);
        assert_eq!(
            ControlMessage::decode(frame.clone(), ProtocolVersion::V22).unwrap(),
            message
        );

        let mut truncated = frame;
        truncated.payload.pop();
        assert!(matches!(
            ControlMessage::decode(truncated, ProtocolVersion::V22),
            Err(CodecError::Truncated {
                message_id: wire_id::START_ANNOUNCEMENT,
                needed: 464,
                actual: 463,
            })
        ));

        for (message, expected_payload_len) in [
            (ControlMessage::StopAnnouncement { conference_id: 42 }, 4),
            (
                ControlMessage::AnnouncementFinish {
                    conference_id: 42,
                    play_status: AnnouncementPlayStatus::Unknown(3),
                },
                8,
            ),
        ] {
            let bytes = message.encode(ProtocolVersion::V22).unwrap();
            let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
            assert_eq!(frame.payload.len(), expected_payload_len);
            assert_eq!(
                ControlMessage::decode(frame, ProtocolVersion::V22).unwrap(),
                message
            );
        }
    }

    #[test]
    fn conference_lifecycle_messages_use_documented_wire_sizes() {
        let server_messages = [
            (
                ControlMessage::ClearConference {
                    conference_id: 42.into(),
                    service_number: 3,
                },
                wire_id::CLEAR_CONFERENCE,
                8,
            ),
            (
                ControlMessage::CreateConferenceRequest(CreateConferenceRequest {
                    conference_id: 42.into(),
                    reserved_participants: 8,
                    resource_type: ConferenceResourceType::Conference,
                    application_id: 7.into(),
                    application_conference_id: "festival-42".into(),
                    application_data: "main-stage".into(),
                    passthrough_data: vec![1, 2, 3],
                }),
                wire_id::CREATE_CONFERENCE_REQ,
                80,
            ),
            (
                ControlMessage::DeleteConferenceRequest {
                    conference_id: 42.into(),
                },
                wire_id::DELETE_CONFERENCE_REQ,
                4,
            ),
            (
                ControlMessage::ModifyConferenceRequest(ModifyConferenceRequest {
                    conference_id: 42.into(),
                    reserved_participants: 12,
                    application_id: 7.into(),
                    application_conference_id: "festival-42".into(),
                    application_data: "main-stage".into(),
                    passthrough_data: vec![4, 5],
                }),
                wire_id::MODIFY_CONFERENCE_REQ,
                76,
            ),
            (
                ControlMessage::AuditConferenceRequest,
                wire_id::AUDIT_CONFERENCE_REQ,
                0,
            ),
        ];
        for (message, expected_id, expected_payload_len) in server_messages {
            let frame = FrameDecoder::new()
                .push(&message.encode(ProtocolVersion::V22).unwrap())
                .unwrap()
                .remove(0);
            assert_eq!(frame.message_id, expected_id);
            assert_eq!(frame.payload.len(), expected_payload_len);
            assert_eq!(
                ControlMessage::decode(frame, ProtocolVersion::V22).unwrap(),
                message
            );
        }

        let audit = AuditConferenceResponse {
            last: 1,
            entries: vec![AuditConferenceEntry {
                conference_id: 42.into(),
                resource_type: ConferenceResourceType::Conference,
                reserved_participants: 8,
                active_participants: 3,
                application_id: 7.into(),
                application_conference_id: "festival-42".into(),
                application_data: "main-stage".into(),
            }],
        };
        let client_messages = [
            (
                ControlMessage::CreateConferenceResponse(CreateConferenceResponse {
                    conference_id: 42.into(),
                    result: CreateConferenceResult::Ok,
                    passthrough_data: vec![1, 2, 3],
                }),
                wire_id::CREATE_CONFERENCE_RES,
                16,
            ),
            (
                ControlMessage::DeleteConferenceResponse {
                    conference_id: 42.into(),
                    result: DeleteConferenceResult::Ok,
                },
                wire_id::DELETE_CONFERENCE_RES,
                8,
            ),
            (
                ControlMessage::ModifyConferenceResponse(ModifyConferenceResponse {
                    conference_id: 42.into(),
                    result: ModifyConferenceResult::Ok,
                    passthrough_data: vec![4, 5],
                }),
                wire_id::MODIFY_CONFERENCE_RES,
                16,
            ),
            (
                ControlMessage::AuditConferenceResponse(audit),
                wire_id::AUDIT_CONFERENCE_RES,
                84,
            ),
        ];
        for (message, expected_id, expected_payload_len) in client_messages {
            let frame = FrameDecoder::new()
                .push(&message.encode(ProtocolVersion::V22).unwrap())
                .unwrap()
                .remove(0);
            assert_eq!(frame.message_id, expected_id);
            assert_eq!(frame.payload.len(), expected_payload_len);
            assert_eq!(
                ControlMessage::decode(frame, ProtocolVersion::V22).unwrap(),
                message
            );
        }
    }

    #[test]
    fn conference_participant_messages_and_application_changes_round_trip() {
        let participant = ConferenceParticipant {
            call_reference: 100.into(),
            presentation_restrictions: PartyInformationRestrictions::CALLING_NUMBER
                | PartyInformationRestrictions::LAST_REDIRECT_NAME,
            name: "Festival Caller".into(),
            number: "1001".into(),
            conference_name: "Main Stage".into(),
        };
        let server_messages = [
            (
                ControlMessage::AddParticipantRequest(AddParticipantRequest {
                    conference_id: 42.into(),
                    participant: participant.clone(),
                }),
                wire_id::ADD_PARTICIPANT_REQ,
                108,
            ),
            (
                ControlMessage::DropParticipantRequest {
                    conference_id: 42.into(),
                    call_reference: 100.into(),
                },
                wire_id::DROP_PARTICIPANT_REQ,
                8,
            ),
            (
                ControlMessage::AuditParticipantRequest {
                    conference_id: 42.into(),
                },
                wire_id::AUDIT_PARTICIPANT_REQ,
                4,
            ),
        ];
        for (message, expected_id, expected_payload_len) in server_messages {
            let frame = FrameDecoder::new()
                .push(&message.encode(ProtocolVersion::V22).unwrap())
                .unwrap()
                .remove(0);
            assert_eq!(frame.message_id, expected_id);
            assert_eq!(frame.payload.len(), expected_payload_len);
            assert_eq!(
                ControlMessage::decode(frame, ProtocolVersion::V22).unwrap(),
                message
            );
        }

        let client_messages = [
            (
                ControlMessage::AddParticipantResponse(AddParticipantResponse {
                    conference_id: 42.into(),
                    call_reference: 100.into(),
                    result: AddParticipantResult::Ok,
                    bridge_participant_id: BoundedBytes::try_from(vec![3; 257]).unwrap(),
                }),
                wire_id::ADD_PARTICIPANT_RES,
                272,
            ),
            (
                ControlMessage::AuditParticipantResponse(AuditParticipantResponse {
                    result: AuditParticipantResult::Ok,
                    last: 1,
                    conference_id: 42.into(),
                    number_of_entries: 2,
                    participant_entries: vec![1, 2, 3, 4],
                }),
                wire_id::AUDIT_PARTICIPANT_RES,
                20,
            ),
        ];
        for (message, expected_id, expected_payload_len) in client_messages {
            let frame = FrameDecoder::new()
                .push(&message.encode(ProtocolVersion::V22).unwrap())
                .unwrap()
                .remove(0);
            assert_eq!(frame.message_id, expected_id);
            assert_eq!(frame.payload.len(), expected_payload_len);
            assert_eq!(
                ControlMessage::decode(frame, ProtocolVersion::V22).unwrap(),
                message
            );
        }

        let change = ConferenceParticipantChange {
            conference_id: 42.into(),
            participant,
        };
        let routing = ParticipantChangeRouting {
            application_id: 7.into(),
            line_instance: 1,
            transaction_id: 9.into(),
            sequence_flag: 1,
            display_priority: 2,
            application_instance_id: 3.into(),
            routing: 4,
        };
        let envelope = change.to_user_data_v1(routing).unwrap();
        assert_eq!(envelope.data.len(), 108);
        assert_eq!(
            ConferenceParticipantChange::from_user_data_v1(&envelope).unwrap(),
            change
        );

        let mut mismatched = envelope;
        mismatched.conference_id += 1;
        assert!(matches!(
            ConferenceParticipantChange::from_user_data_v1(&mismatched),
            Err(CodecError::InvalidValue {
                field: "participant change conference ID",
                ..
            })
        ));
    }

    #[test]
    fn participant_messages_enforce_text_and_audit_bounds() {
        let oversized = ControlMessage::AuditParticipantResponse(AuditParticipantResponse {
            result: AuditParticipantResult::Ok,
            last: 1,
            conference_id: 42.into(),
            number_of_entries: 1,
            participant_entries: vec![0; 257],
        });
        assert!(matches!(
            oversized.encode(ProtocolVersion::V22),
            Err(CodecError::CountTooLarge {
                field: "participant audit data",
                count: 257,
                maximum: 256,
                ..
            })
        ));

        let mut oversized_payload = vec![0; 16 + 257];
        oversized_payload[8..12].copy_from_slice(&42_u32.to_le_bytes());
        assert!(matches!(
            ControlMessage::decode(
                Frame::new(22, wire_id::AUDIT_PARTICIPANT_RES, oversized_payload),
                ProtocolVersion::V22,
            ),
            Err(CodecError::CountTooLarge {
                field: "participant audit data",
                count: 257,
                maximum: 256,
                ..
            })
        ));

        let long_name = ControlMessage::AddParticipantRequest(AddParticipantRequest {
            conference_id: 42.into(),
            participant: ConferenceParticipant {
                call_reference: 100.into(),
                presentation_restrictions: PartyInformationRestrictions::empty(),
                name: "x".repeat(40),
                number: "1001".into(),
                conference_name: "Main Stage".into(),
            },
        });
        assert!(matches!(
            long_name.encode(ProtocolVersion::V22),
            Err(CodecError::TextTooLong {
                field: "participant name",
                actual: 40,
                maximum: 39,
                ..
            })
        ));
    }

    #[test]
    fn multicast_media_layouts_cover_legacy_and_extended_addresses() {
        let acknowledgement = ClientMessage::MulticastMediaReceptionAck {
            status: MediaStatus::Ok,
            passthrough_party_id: 9.into(),
            call_reference: 7.into(),
        };
        let frame = FrameDecoder::new()
            .push(&acknowledgement.encode(ProtocolVersion::V3).unwrap())
            .unwrap()
            .remove(0);
        assert_eq!(frame.message_id, wire_id::MULTICAST_MEDIA_RECEPTION_ACK);
        assert_eq!(frame.payload.len(), 12);
        assert_eq!(ClientMessage::decode(frame).unwrap(), acknowledgement);

        let reception_v3 = ServerMessage::StartMulticastMediaReception(MulticastMediaReception {
            conference_id: 42.into(),
            passthrough_party_id: 9.into(),
            call_reference: 7.into(),
            address: "239.1.2.3".parse().unwrap(),
            port: 16_000,
            packet_millis: 20,
            codec: Codec::Pcmu,
            echo_cancellation: EchoCancellation::On,
            g723_bitrate: G723BitRate::Rate6_3,
        });
        let transmission_v3 =
            ServerMessage::StartMulticastMediaTransmission(MulticastMediaTransmission {
                conference_id: 42.into(),
                passthrough_party_id: 9.into(),
                call_reference: 7.into(),
                address: "239.1.2.3".parse().unwrap(),
                port: 16_002,
                packet_millis: 20,
                codec: Codec::Pcmu,
                precedence: 5,
                silence_suppression: 1,
                max_frames_per_packet: 2,
                g723_bitrate: G723BitRate::Rate5_3,
            });
        for (message, expected_id, expected_payload_len) in [
            (reception_v3, wire_id::START_MULTICAST_MEDIA_RECEPTION, 36),
            (
                transmission_v3,
                wire_id::START_MULTICAST_MEDIA_TRANSMISSION,
                44,
            ),
        ] {
            let frame = FrameDecoder::new()
                .push(&message.encode(ProtocolVersion::V3).unwrap())
                .unwrap()
                .remove(0);
            assert_eq!(frame.message_id, expected_id);
            assert_eq!(frame.payload.len(), expected_payload_len);
            assert_eq!(
                ServerMessage::decode(frame, ProtocolVersion::V3).unwrap(),
                message
            );
        }

        let reception_v17 = ServerMessage::StartMulticastMediaReception(MulticastMediaReception {
            conference_id: 43.into(),
            passthrough_party_id: 10.into(),
            call_reference: 8.into(),
            address: "ff3e::1234".parse().unwrap(),
            port: 17_000,
            packet_millis: 30,
            codec: Codec::Pcma,
            echo_cancellation: EchoCancellation::Unknown(7),
            g723_bitrate: G723BitRate::Unknown(9),
        });
        let transmission_v17 =
            ServerMessage::StartMulticastMediaTransmission(MulticastMediaTransmission {
                conference_id: 43.into(),
                passthrough_party_id: 10.into(),
                call_reference: 8.into(),
                address: "ff3e::1234".parse().unwrap(),
                port: 17_002,
                packet_millis: 30,
                codec: Codec::Pcma,
                precedence: 6,
                silence_suppression: 2,
                max_frames_per_packet: 3,
                g723_bitrate: G723BitRate::Unknown(9),
            });
        for (message, expected_id, expected_payload_len) in [
            (reception_v17, wire_id::START_MULTICAST_MEDIA_RECEPTION, 52),
            (
                transmission_v17,
                wire_id::START_MULTICAST_MEDIA_TRANSMISSION,
                60,
            ),
        ] {
            let frame = FrameDecoder::new()
                .push(&message.encode(ProtocolVersion::V17).unwrap())
                .unwrap()
                .remove(0);
            assert_eq!(frame.message_id, expected_id);
            assert_eq!(frame.payload.len(), expected_payload_len);
            assert_eq!(
                ServerMessage::decode(frame, ProtocolVersion::V17).unwrap(),
                message
            );
        }

        for (message, expected_id) in [
            (
                ServerMessage::StopMulticastMediaReception {
                    conference_id: 42.into(),
                    passthrough_party_id: 9.into(),
                    call_reference: 100.into(),
                },
                wire_id::STOP_MULTICAST_MEDIA_RECEPTION,
            ),
            (
                ServerMessage::StopMulticastMediaTransmission {
                    conference_id: 42.into(),
                    passthrough_party_id: 9.into(),
                    call_reference: 100.into(),
                },
                wire_id::STOP_MULTICAST_MEDIA_TRANSMISSION,
            ),
        ] {
            let frame = FrameDecoder::new()
                .push(&message.encode(ProtocolVersion::V22).unwrap())
                .unwrap()
                .remove(0);
            assert_eq!(frame.message_id, expected_id);
            assert_eq!(frame.payload.len(), 12);
            assert_eq!(
                ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
                message
            );
        }
    }

    #[test]
    fn legacy_multicast_rejects_ipv6_and_invalid_ports() {
        let ipv6 = ServerMessage::StartMulticastMediaReception(MulticastMediaReception {
            conference_id: 42.into(),
            passthrough_party_id: 9.into(),
            call_reference: 7.into(),
            address: "ff3e::1234".parse().unwrap(),
            port: 16_000,
            packet_millis: 20,
            codec: Codec::Pcmu,
            echo_cancellation: EchoCancellation::On,
            g723_bitrate: G723BitRate::Rate6_3,
        });
        assert!(matches!(
            ipv6.encode(ProtocolVersion::V15),
            Err(CodecError::InvalidValue {
                field: "IP address family for pre-v17 protocol",
                ..
            })
        ));

        let mut payload = vec![0; 52];
        payload[8..12].copy_from_slice(&1_u32.to_le_bytes());
        payload[28..32].copy_from_slice(&70_000_u32.to_le_bytes());
        assert!(matches!(
            ServerMessage::decode(
                Frame::new(17, wire_id::START_MULTICAST_MEDIA_RECEPTION, payload),
                ProtocolVersion::V17,
            ),
            Err(CodecError::InvalidValue {
                field: "multicast port",
                value: 70_000,
                ..
            })
        ));
    }

    #[test]
    fn qos_control_messages_use_field_typed_service_layouts() {
        let flow = QosFlow {
            conference_id: 42.into(),
            call_reference: 7.into(),
            passthrough_party_id: 9.into(),
            address: "192.0.2.20".parse().unwrap(),
            port: 16_000,
        };
        let traffic = QosTrafficSpecification {
            codec: Codec::Pcmu,
            average_bit_rate: 64_000,
            burst_size: 1_200,
            peak_rate: 128_000,
        };
        let application = QosApplicationIdentifier {
            vendor_id: "Cisco".into(),
            version: "1".into(),
            application_name: "SCCP audio".into(),
            sub_application_id: "primary".into(),
        };
        let messages = [
            ControlMessage::QosReservationNotify {
                flow,
                direction: QosDirection::Send,
            },
            ControlMessage::QosErrorNotify {
                flow,
                direction: QosDirection::Send,
                error_code: QosErrorCode::ListenFailed,
                failure_node: "198.51.100.9".parse().unwrap(),
                rsvp_error_code: RsvpErrorCode::NoSenderInformation,
                rsvp_error_subcode: 5,
                rsvp_error_flags: 6,
            },
            ControlMessage::QosListen {
                flow,
                reservation_style: QosReservationStyle::SharedExplicit,
                maximum_retries: 3,
                retry_timer: 4,
                confirmation_required: true,
                preemption_priority: 5,
                defending_priority: 6,
                traffic,
                application: application.clone(),
            },
            ControlMessage::QosPath {
                flow,
                reservation_style: QosReservationStyle::SharedExplicit,
                maximum_retries: 3,
                retry_timer: 4,
                preemption_priority: 5,
                defending_priority: 6,
                traffic,
                application: application.clone(),
            },
            ControlMessage::QosTeardown {
                flow,
                direction: QosDirection::Send,
            },
            ControlMessage::UpdateDscp { flow, dscp: 46 },
            ControlMessage::QosModify {
                flow,
                direction: QosDirection::Send,
                traffic,
                application,
            },
        ];
        for (message, expected_size) in messages.into_iter().zip([24, 44, 172, 168, 24, 24, 152]) {
            let bytes = message.encode(ProtocolVersion::V22).unwrap();
            let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
            assert_eq!(frame.payload.len(), expected_size);
            assert_eq!(
                ControlMessage::decode(frame, ProtocolVersion::V22).unwrap(),
                message
            );
        }
    }

    #[test]
    fn fixed_layout_alignment_bytes_must_be_zero() {
        let register_ack = WireRegisterAck {
            keepalive_seconds: 30,
            date_template: *b"D/M/Y\0",
            alignment: [1, 0],
            secondary_keepalive_seconds: 30,
            protocol_features: [22, 0, 0, 0],
        };
        assert!(
            ServerMessage::decode(
                Frame::new(
                    ProtocolVersion::V22.wire(),
                    wire_id::REGISTER_ACK,
                    encode(wire_id::REGISTER_ACK, &register_ack).unwrap(),
                ),
                ProtocolVersion::V22,
            )
            .is_err()
        );

        let notification = WireMessageWaitingNotification {
            target_number: WireFixedText::new(wire_id::MWI_NOTIFICATION, "target", "1001").unwrap(),
            control_number: WireFixedText::new(wire_id::MWI_NOTIFICATION, "control", "5000")
                .unwrap(),
            alignment: [1, 0],
            messages_waiting: 1,
            total_voicemail_new: 0,
            total_voicemail_old: 0,
            priority_voicemail_new: 0,
            priority_voicemail_old: 0,
            total_fax_new: 0,
            total_fax_old: 0,
            priority_fax_new: 0,
            priority_fax_old: 0,
        };
        let notification = encode(wire_id::MWI_NOTIFICATION, &notification).unwrap();
        assert_eq!(notification.len(), 88);
        assert!(
            ControlMessage::decode(
                Frame::new(
                    ProtocolVersion::V22.wire(),
                    wire_id::MWI_NOTIFICATION,
                    notification,
                ),
                ProtocolVersion::V22,
            )
            .is_err()
        );

        let response = WireMessageWaitingResponse {
            target_number: WireFixedText::new(wire_id::MWI_RESPONSE, "target", "1001").unwrap(),
            alignment: [0, 1, 0],
            result: MessageWaitingResult::Ok.wire_value(),
        };
        let response = encode(wire_id::MWI_RESPONSE, &response).unwrap();
        assert_eq!(response.len(), 32);
        assert!(
            ControlMessage::decode(
                Frame::new(ProtocolVersion::V22.wire(), wire_id::MWI_RESPONSE, response,),
                ProtocolVersion::V22,
            )
            .is_err()
        );

        let connection_statistics = WireConnectionStatisticsV19 {
            directory_number: WireAlignedText {
                value: WireFixedText::new(
                    wire_id::CONNECTION_STATISTICS_RES,
                    "directory number",
                    "2002",
                )
                .unwrap(),
                alignment: [1, 0, 0],
            },
            call_reference: 42,
            processing: StatisticsProcessing::Clear.wire_value(),
            statistics: WireConnectionStatisticsTail {
                counters: WireConnectionStatisticsCounters {
                    packets_sent: 0,
                    octets_sent: 0,
                    packets_received: 0,
                    octets_received: 0,
                    packets_lost: 0,
                    jitter_millis: 0,
                    latency_millis: 0,
                },
                quality_size: 0,
            },
            quality: Vec::new(),
        };
        assert!(
            ClientMessage::decode_with_version(
                Frame::new(
                    ProtocolVersion::V20.wire(),
                    wire_id::CONNECTION_STATISTICS_RES,
                    encode(wire_id::CONNECTION_STATISTICS_RES, &connection_statistics).unwrap(),
                ),
                ProtocolVersion::V20,
            )
            .is_err()
        );

        let mut enbloc = encode(
            wire_id::ENBLOC_CALL,
            &WireEnblocWithLine::<25, 0> {
                called_party: WireAlignedText::new(wire_id::ENBLOC_CALL, "called party", "2001")
                    .unwrap(),
                line_instance: 2,
            },
        )
        .unwrap();
        enbloc.extend_from_slice(&[1, 0, 0]);
        assert!(
            ClientMessage::decode_with_version(
                Frame::new(ProtocolVersion::V19.wire(), wire_id::ENBLOC_CALL, enbloc),
                ProtocolVersion::V19,
            )
            .is_err()
        );

        let mut off_hook = FrameDecoder::new()
            .push(
                &ClientMessage::OffHookWithCallingParty {
                    calling_party_number: "2001".into(),
                    voice_mailbox: "5000".into(),
                    line_instance: 2,
                }
                .encode(ProtocolVersion::V19)
                .unwrap(),
            )
            .unwrap()
            .remove(0);
        off_hook.payload[50] = 1;
        assert!(ClientMessage::decode_with_version(off_hook, ProtocolVersion::V19).is_err());

        let mut dialed = FrameDecoder::new()
            .push(
                &ServerMessage::DialedNumber {
                    number: "2001".into(),
                    line_instance: 2,
                    call_reference: 42,
                }
                .encode(ProtocolVersion::V19)
                .unwrap(),
            )
            .unwrap()
            .remove(0);
        dialed.payload[25] = 1;
        assert!(ServerMessage::decode(dialed, ProtocolVersion::V19).is_err());

        let mut forwarding = FrameDecoder::new()
            .push(
                &ServerMessage::ForwardStatus {
                    line_instance: 2,
                    forward_all: Some("2001".into()),
                    forward_busy: None,
                    forward_no_answer: None,
                }
                .encode(ProtocolVersion::V19)
                .unwrap(),
            )
            .unwrap()
            .remove(0);
        forwarding.payload[37] = 1;
        assert!(ServerMessage::decode(forwarding, ProtocolVersion::V19).is_err());
    }

    #[test]
    fn boolean_control_words_reject_non_boolean_values() {
        let recording = encode(
            wire_id::RECORDING_STATUS,
            &WireRecordingStatus {
                call_reference: 7,
                active: 2,
            },
        )
        .unwrap();
        assert!(matches!(
            ServerMessage::decode(
                Frame::new(
                    ProtocolVersion::V22.wire(),
                    wire_id::RECORDING_STATUS,
                    recording
                ),
                ProtocolVersion::V22,
            ),
            Err(CodecError::InvalidValue {
                field: "recording active",
                value: 2,
                ..
            })
        ));

        let notification = WireMessageWaitingNotification {
            target_number: WireFixedText::new(wire_id::MWI_NOTIFICATION, "target", "1001").unwrap(),
            control_number: WireFixedText::new(wire_id::MWI_NOTIFICATION, "control", "5000")
                .unwrap(),
            alignment: [0; 2],
            messages_waiting: 2,
            total_voicemail_new: 0,
            total_voicemail_old: 0,
            priority_voicemail_new: 0,
            priority_voicemail_old: 0,
            total_fax_new: 0,
            total_fax_old: 0,
            priority_fax_new: 0,
            priority_fax_old: 0,
        };
        let payload = encode(wire_id::MWI_NOTIFICATION, &notification).unwrap();
        assert!(matches!(
            ControlMessage::decode(
                Frame::new(
                    ProtocolVersion::V22.wire(),
                    wire_id::MWI_NOTIFICATION,
                    payload
                ),
                ProtocolVersion::V22,
            ),
            Err(CodecError::InvalidValue {
                field: "messages waiting",
                value: 2,
                ..
            })
        ));

        let flow = QosFlow {
            conference_id: 1.into(),
            call_reference: 2.into(),
            passthrough_party_id: 3.into(),
            address: "192.0.2.1".parse().unwrap(),
            port: 16_000,
        };
        let traffic = QosTrafficSpecification {
            codec: Codec::Pcmu,
            average_bit_rate: 64_000,
            burst_size: 1_200,
            peak_rate: 128_000,
        };
        let application = QosApplicationIdentifier {
            vendor_id: "Cisco".into(),
            version: "1".into(),
            application_name: "SCCP audio".into(),
            sub_application_id: "primary".into(),
        };
        let mut qos_listen = FrameDecoder::new()
            .push(
                &ControlMessage::QosListen {
                    flow,
                    reservation_style: QosReservationStyle::SharedExplicit,
                    maximum_retries: 3,
                    retry_timer: 4,
                    confirmation_required: true,
                    preemption_priority: 5,
                    defending_priority: 6,
                    traffic,
                    application,
                }
                .encode(ProtocolVersion::V22)
                .unwrap(),
            )
            .unwrap()
            .remove(0);
        qos_listen.payload[32..36].copy_from_slice(&2_u32.to_le_bytes());
        assert!(matches!(
            ControlMessage::decode(qos_listen, ProtocolVersion::V22),
            Err(CodecError::InvalidValue {
                field: "QoS confirmation required",
                value: 2,
                ..
            })
        ));

        assert!(matches!(
            ControlMessage::UpdateDscp { flow, dscp: 64 }.encode(ProtocolVersion::V22),
            Err(CodecError::InvalidValue {
                field: "DSCP",
                value: 64,
                ..
            })
        ));

        let invalid_dscp = encode(
            wire_id::UPDATE_DSCP,
            &WireUpdateDscp {
                flow: qos_flow_to_wire(flow),
                dscp: 64,
            },
        )
        .unwrap();
        assert!(matches!(
            ControlMessage::decode(
                Frame::new(
                    ProtocolVersion::V22.wire(),
                    wire_id::UPDATE_DSCP,
                    invalid_dscp
                ),
                ProtocolVersion::V22,
            ),
            Err(CodecError::InvalidValue {
                field: "DSCP",
                value: 64,
                ..
            })
        ));
    }

    #[test]
    fn compact_multimedia_dtmf_and_addon_layouts_round_trip_exactly() {
        let open_ack = OpenMultimediaReceiveChannelAck {
            status: MediaStatus::Ok,
            endpoint: MediaEndpointAddress {
                address: "2001:db8::20".parse().unwrap(),
                port: 16_000,
            },
            passthrough_party_id: 9.into(),
            call_reference: 7.into(),
        };
        let start_ack = StartMultimediaTransmissionAck {
            conference_id: 42.into(),
            passthrough_party_id: 9.into(),
            call_reference: 7.into(),
            endpoint: MediaEndpointAddress {
                address: "2001:db8::20".parse().unwrap(),
                port: 16_000,
            },
            status: MediaStatus::Ok,
        };
        for (message, expected_len) in [
            (ClientMessage::OpenMultimediaReceiveChannelAck(open_ack), 48),
            (ClientMessage::StartMultimediaTransmissionAck(start_ack), 52),
        ] {
            let bytes = message.encode(ProtocolVersion::V22).unwrap();
            assert_eq!(bytes.len(), expected_len);
            let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
            assert_eq!(ClientMessage::decode(frame).unwrap(), message);
        }

        let addon = ClientMessage::ExtensionDeviceCapabilities(ExtensionDeviceCapabilities {
            unknown_1: 1,
            unknown_2: 2,
            unknown_3: 3,
            description: "7914 sidecar".into(),
        });
        let bytes = addon.encode(ProtocolVersion::V22).unwrap();
        assert_eq!(bytes.len(), 176);
        assert_eq!(
            ClientMessage::decode(FrameDecoder::new().push(&bytes).unwrap().remove(0)).unwrap(),
            addon
        );

        let dtmf = DtmfToneControl {
            tone: Tone::Dtmf5,
            conference_id: 42.into(),
            passthrough_party_id: 9,
        };
        for message in [
            ServerMessage::NotifyDtmfTone(dtmf),
            ServerMessage::SendDtmfTone(dtmf),
        ] {
            let bytes = message.encode(ProtocolVersion::V22).unwrap();
            assert_eq!(bytes.len(), 24);
            let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
            assert_eq!(
                ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
                message
            );
        }

        let lifecycle = MultimediaStreamControl {
            conference_id: 42.into(),
            passthrough_party_id: 9.into(),
            call_reference: 7.into(),
            port_handling_flag: 1,
        };
        let flow = VideoFlowControl {
            conference_id: 42.into(),
            passthrough_party_id: 9.into(),
            call_reference: 7.into(),
            maximum_bit_rate: 512_000,
        };
        for message in [
            ServerMessage::StopMultimediaTransmission(lifecycle),
            ServerMessage::CloseMultimediaReceiveChannel(lifecycle),
            ServerMessage::FlowControlCommand(flow),
            ServerMessage::FlowControlNotify(flow),
        ] {
            let bytes = message.encode(ProtocolVersion::V22).unwrap();
            assert_eq!(bytes.len(), 28);
            let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
            assert_eq!(
                ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
                message
            );
        }
        let display = ServerMessage::VideoDisplayCommand {
            conference_id: 42.into(),
            call_reference: 7.into(),
            layout_id: 2,
        };
        let bytes = display.encode(ProtocolVersion::V22).unwrap();
        assert_eq!(bytes.len(), 24);
        let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
        assert_eq!(
            ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
            display
        );

        let failure_detection = ServerMessage::StartMediaFailureDetection(MediaFailureDetection {
            conference_id: 42.into(),
            passthrough_party_id: 9,
            packet_millis: 20,
            codec: Codec::Pcmu,
            echo_cancellation: EchoCancellation::On,
            codec_qualifier: [1, 2, 3, 4],
            call_reference: 7.into(),
        });
        let bytes = failure_detection.encode(ProtocolVersion::V22).unwrap();
        assert_eq!(bytes.len(), 40);
        let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
        assert_eq!(
            ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
            failure_detection
        );

        let dynamic = ServerMessage::ConfigStatus(ConfigurationStatus {
            device_name: "SEP001122334455".into(),
            station_user_id: 0xfeed,
            station_instance: 2,
            line_count: 6,
            speed_dial_count: 12,
            user_name: "festival".into(),
            server_name: "sccp.example.test".into(),
        });
        let bytes = dynamic.encode(ProtocolVersion::V22).unwrap();
        assert_eq!(bytes.len() % 4, 0);
        let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
        assert_eq!(
            ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
            dynamic
        );
    }

    #[test]
    fn soft_key_template_keeps_cisco_event_positions() {
        let bytes = ServerMessage::SoftKeyTemplate {
            actions: SoftKeyProfile::default().template_actions(),
        }
        .encode(ProtocolVersion::V22)
        .unwrap();
        let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
        let payload: WireSoftKeyTemplate = decode(frame.message_id, &frame.payload).unwrap();
        assert_eq!(payload.count, 32);
        assert_eq!(payload.definitions[31].event, 32);
        assert_eq!(
            payload
                .definitions
                .iter()
                .map(|definition| definition.event)
                .collect::<Vec<_>>(),
            (1..=32).collect::<Vec<_>>()
        );
    }

    #[test]
    fn line_only_button_template_preserves_the_fixed_wire_layout() {
        let message = ServerMessage::ButtonTemplate {
            offset: 0,
            total: 1,
            buttons: vec![ButtonTemplateEntry {
                instance: 1,
                button_type: ButtonType::Line,
            }],
        };
        let bytes = message.encode(ProtocolVersion::V22).unwrap();
        let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
        let payload: WireButtonTemplate = decode(frame.message_id, &frame.payload).unwrap();

        assert_eq!(payload.offset, 0);
        assert_eq!(payload.count, 1);
        assert_eq!(payload.total, 1);
        assert_eq!(
            payload.definitions[0],
            WireButtonDefinition {
                instance: 1,
                button_type: ButtonType::Line.wire_value() as u8,
            }
        );
        assert!(
            payload.definitions[1..]
                .iter()
                .all(|definition| *definition == WireButtonDefinition::default())
        );
        assert_eq!(
            ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
            message
        );
    }

    #[test]
    fn mixed_button_template_round_trips_ordered_semantic_entries() {
        let message = ServerMessage::ButtonTemplate {
            offset: BUTTON_TEMPLATE_ENTRIES_PER_CHUNK as u32,
            total: BUTTON_TEMPLATE_ENTRIES_PER_CHUNK as u32 + 6,
            buttons: vec![
                ButtonTemplateEntry {
                    instance: 1,
                    button_type: ButtonType::Line,
                },
                ButtonTemplateEntry {
                    instance: 2,
                    button_type: ButtonType::SpeedDial,
                },
                ButtonTemplateEntry {
                    instance: 3,
                    button_type: ButtonType::DoNotDisturb,
                },
                ButtonTemplateEntry {
                    instance: 4,
                    button_type: ButtonType::ServiceUrl,
                },
                ButtonTemplateEntry {
                    instance: 0,
                    button_type: ButtonType::Unused,
                },
                ButtonTemplateEntry {
                    instance: 5,
                    button_type: ButtonType::BlfSpeedDial,
                },
            ],
        };

        let bytes = message.encode(ProtocolVersion::V22).unwrap();
        let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
        assert_eq!(
            ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
            message
        );
    }

    #[test]
    fn button_template_rejects_unrepresentable_entries_and_counts() {
        let too_many = ServerMessage::ButtonTemplate {
            offset: 0,
            total: BUTTON_TEMPLATE_ENTRIES_PER_CHUNK as u32 + 1,
            buttons: vec![
                ButtonTemplateEntry {
                    instance: 1,
                    button_type: ButtonType::Line,
                };
                BUTTON_TEMPLATE_ENTRIES_PER_CHUNK + 1
            ],
        };
        assert!(matches!(
            too_many.encode(ProtocolVersion::V22),
            Err(CodecError::CountTooLarge { .. })
        ));

        let large_instance = ServerMessage::ButtonTemplate {
            offset: 0,
            total: 1,
            buttons: vec![ButtonTemplateEntry {
                instance: 256,
                button_type: ButtonType::Line,
            }],
        };
        assert!(matches!(
            large_instance.encode(ProtocolVersion::V22),
            Err(CodecError::InvalidValue {
                field: "button instance",
                ..
            })
        ));

        let payload = WireButtonTemplate {
            offset: 0,
            count: BUTTON_TEMPLATE_ENTRIES_PER_CHUNK as u32 + 1,
            total: BUTTON_TEMPLATE_ENTRIES_PER_CHUNK as u32,
            definitions: [WireButtonDefinition::default(); BUTTON_TEMPLATE_ENTRIES_PER_CHUNK],
        };
        let frame = Frame::new(
            ProtocolVersion::V22.wire(),
            wire_id::BUTTON_TEMPLATE,
            encode(wire_id::BUTTON_TEMPLATE, &payload).unwrap(),
        );
        assert!(matches!(
            ServerMessage::decode(frame, ProtocolVersion::V22),
            Err(CodecError::CountTooLarge {
                field: "button definitions in message",
                ..
            })
        ));

        let payload = WireButtonTemplate {
            offset: 1,
            count: BUTTON_TEMPLATE_ENTRIES_PER_CHUNK as u32,
            total: BUTTON_TEMPLATE_ENTRIES_PER_CHUNK as u32,
            definitions: [WireButtonDefinition::default(); BUTTON_TEMPLATE_ENTRIES_PER_CHUNK],
        };
        let frame = Frame::new(
            ProtocolVersion::V22.wire(),
            wire_id::BUTTON_TEMPLATE,
            encode(wire_id::BUTTON_TEMPLATE, &payload).unwrap(),
        );
        assert!(matches!(
            ServerMessage::decode(frame, ProtocolVersion::V22),
            Err(CodecError::InvalidValue {
                field: "button template range",
                ..
            })
        ));
    }

    #[test]
    fn station_statuses_select_and_round_trip_dynamic_layouts() {
        let cases = [
            (
                ServerMessage::LineStatus {
                    instance: 3,
                    directory_number: "1003".into(),
                    fully_qualified_display_name: "Desk 1003".into(),
                    display_label: "A dynamic line label".into(),
                },
                ProtocolVersion::V8,
                wire_id::LINE_STAT,
            ),
            (
                ServerMessage::LineStatus {
                    instance: 3,
                    directory_number: "1003".into(),
                    fully_qualified_display_name: "Desk 1003".into(),
                    display_label: "A dynamic line label".into(),
                },
                ProtocolVersion::V9,
                wire_id::LINE_STAT_DYNAMIC,
            ),
            (
                ServerMessage::SpeedDialStatus {
                    instance: 4,
                    number: "2004".into(),
                    display_name: "Warehouse".into(),
                },
                ProtocolVersion::V8,
                wire_id::SPEED_DIAL_STAT,
            ),
            (
                ServerMessage::SpeedDialStatus {
                    instance: 4,
                    number: "2004".into(),
                    display_name: "Warehouse".into(),
                },
                ProtocolVersion::V9,
                wire_id::SPEED_DIAL_STAT_DYNAMIC,
            ),
            (
                ServerMessage::FeatureStatus {
                    instance: 5,
                    button_type: ButtonType::DoNotDisturb,
                    label: "Do not disturb".into(),
                    state: 0x0002_0101,
                },
                ProtocolVersion::V22,
                wire_id::FEATURE_STAT,
            ),
            (
                ServerMessage::ServiceUrlStatus {
                    index: 6,
                    url: "http://services.invalid/directory".into(),
                    label: "Directory".into(),
                    extension_text: String::new(),
                },
                ProtocolVersion::V8,
                wire_id::SERVICE_URL_STAT,
            ),
            (
                ServerMessage::ServiceUrlStatus {
                    index: 6,
                    url: "http://services.invalid/directory".into(),
                    label: "Directory".into(),
                    extension_text: String::new(),
                },
                ProtocolVersion::V9,
                wire_id::SERVICE_URL_STAT_DYNAMIC,
            ),
        ];

        for (message, protocol, expected_id) in cases {
            let bytes = message.encode(protocol).unwrap();
            assert_eq!(bytes.len() % 4, 0, "message 0x{expected_id:04x}");
            let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
            assert_eq!(frame.message_id, expected_id);
            assert_eq!(ServerMessage::decode(frame, protocol).unwrap(), message);
        }

        let message = ServerMessage::FeatureStatus {
            instance: 5,
            button_type: ButtonType::DoNotDisturb,
            label: "Do not disturb".into(),
            state: 0x0002_0101,
        };
        let session =
            StationSessionContext::new(ProtocolVersion::V8, PhoneFeatures::DYNAMIC_MESSAGES);
        let bytes = message.encode_for_session(session).unwrap();
        let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
        assert_eq!(frame.message_id, wire_id::FEATURE_STAT_DYNAMIC);
        assert_eq!(
            ServerMessage::decode(frame, ProtocolVersion::V8).unwrap(),
            message
        );
    }

    #[test]
    fn line_status_keeps_display_identity_separate_from_button_label() {
        let message = ServerMessage::LineStatus {
            instance: 1,
            directory_number: "coral".into(),
            fully_qualified_display_name: "Coral's phone".into(),
            display_label: "ATP".into(),
        };

        let fixed = message.encode(ProtocolVersion::V8).unwrap();
        let fixed = FrameDecoder::new().push(&fixed).unwrap().remove(0);
        let fixed: WireLineStatus = decode(fixed.message_id, &fixed.payload).unwrap();
        assert_eq!(fixed.directory_number.text().unwrap(), "coral");
        assert_eq!(fixed.display_name.text().unwrap(), "Coral's phone");
        assert_eq!(fixed.display_label.text().unwrap(), "ATP");

        let dynamic = message.encode(ProtocolVersion::V9).unwrap();
        let dynamic = FrameDecoder::new().push(&dynamic).unwrap().remove(0);
        assert_eq!(
            decode_dynamic_texts(dynamic.message_id, &dynamic.payload, 8, 3).unwrap(),
            ["coral", "Coral's phone", "ATP"]
        );
    }

    #[test]
    fn configuration_status_is_lossless_across_session_selected_layouts() {
        let message = ServerMessage::ConfigStatus(ConfigurationStatus {
            device_name: "SEP001122334455".into(),
            station_user_id: 17,
            station_instance: 2,
            line_count: 6,
            speed_dial_count: 12,
            user_name: "festival".into(),
            server_name: "sccp.example.test".into(),
        });
        for (session, expected_id) in [
            (
                StationSessionContext::from(ProtocolVersion::V8),
                wire_id::CONFIG_STAT,
            ),
            (
                StationSessionContext::new(ProtocolVersion::V8, PhoneFeatures::DYNAMIC_MESSAGES),
                wire_id::CONFIG_STAT_DYNAMIC,
            ),
            (
                StationSessionContext::from(ProtocolVersion::V9),
                wire_id::CONFIG_STAT_DYNAMIC,
            ),
        ] {
            let bytes = message.encode_for_session(session).unwrap();
            let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
            assert_eq!(frame.message_id, expected_id);
            assert_eq!(
                ServerMessage::decode(frame, session.protocol).unwrap(),
                message
            );
        }
    }

    #[test]
    fn speed_dial_status_uses_session_selection_and_variable_wire_layout() {
        let message = ServerMessage::SpeedDialStatus {
            instance: 4,
            number: "2004".into(),
            display_name: "Warehouse".into(),
        };
        for (session, expected_id) in [
            (
                StationSessionContext::from(ProtocolVersion::V8),
                wire_id::SPEED_DIAL_STAT,
            ),
            (
                StationSessionContext::new(ProtocolVersion::V8, PhoneFeatures::DYNAMIC_MESSAGES),
                wire_id::SPEED_DIAL_STAT_DYNAMIC,
            ),
            (
                StationSessionContext::from(ProtocolVersion::V9),
                wire_id::SPEED_DIAL_STAT_DYNAMIC,
            ),
        ] {
            let bytes = message.encode_for_session(session).unwrap();
            let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
            assert_eq!(frame.message_id, expected_id);
            assert_eq!(
                ServerMessage::decode(frame, session.protocol).unwrap(),
                message
            );
        }

        let bytes = ServerMessage::SpeedDialStatus {
            instance: 2,
            number: "2001".into(),
            display_name: "Reception".into(),
        }
        .encode(ProtocolVersion::V9)
        .unwrap();
        let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
        assert_eq!(frame.message_id, wire_id::SPEED_DIAL_STAT_DYNAMIC);
        assert_eq!(
            frame.payload,
            [
                0x02, 0x00, 0x00, 0x00, b'2', b'0', b'0', b'1', 0x00, b'R', b'e', b'c', b'e', b'p',
                b't', b'i', b'o', b'n', 0x00, 0x00,
            ]
        );
    }

    #[test]
    fn dynamic_call_information_uses_the_versioned_string_count() {
        let message = ServerMessage::CallInfo {
            info: CallInfo {
                direction: crate::types::CallDirection::Inbound,
                calling_name: "Alice".into(),
                calling_number: "1001".into(),
                called_name: "Bob".into(),
                called_number: "2001".into(),
                original_called_name: "Carol".into(),
                original_called_number: "3001".into(),
                last_redirecting_name: "Dave".into(),
                last_redirecting_number: "4001".into(),
                original_redirect_reason: 2,
                last_redirect_reason: 4,
                party_restrictions: 0,
            },
            line_instance: 2,
            call_reference: 42,
        };

        for (protocol, count) in [
            (ProtocolVersion::V15, 12),
            (ProtocolVersion::V16, 13),
            (ProtocolVersion::V18, 13),
            (ProtocolVersion::V19, 15),
        ] {
            let bytes = message.encode(protocol).unwrap();
            let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
            assert_eq!(frame.message_id, wire_id::CALL_INFO_DYNAMIC);
            assert_eq!(
                decode_dynamic_texts(frame.message_id, &frame.payload, 32, count)
                    .unwrap()
                    .len(),
                count
            );
            assert_eq!(ServerMessage::decode(frame, protocol).unwrap(), message);
        }
    }

    #[test]
    fn dynamic_service_status_adds_the_extension_field_from_version_nineteen() {
        let unsupported = ServerMessage::ServiceUrlStatus {
            index: 3,
            url: "http://services.invalid/directory".into(),
            label: "Directory".into(),
            extension_text: "extension".into(),
        };
        assert!(matches!(
            unsupported.encode(ProtocolVersion::V18),
            Err(CodecError::InvalidValue {
                field: "service URL extension for this protocol version",
                ..
            })
        ));

        let before = ServerMessage::ServiceUrlStatus {
            index: 3,
            url: "http://services.invalid/directory".into(),
            label: "Directory".into(),
            extension_text: String::new(),
        };
        let bytes = before.encode(ProtocolVersion::V18).unwrap();
        let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
        assert_eq!(
            decode_dynamic_texts(frame.message_id, &frame.payload, 4, 2).unwrap(),
            ["http://services.invalid/directory", "Directory"]
        );
        assert_eq!(
            ServerMessage::decode(frame, ProtocolVersion::V18).unwrap(),
            before
        );

        let from = ServerMessage::ServiceUrlStatus {
            index: 3,
            url: "http://services.invalid/directory".into(),
            label: "Directory".into(),
            extension_text: "extension".into(),
        };
        let bytes = from.encode(ProtocolVersion::V19).unwrap();
        let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
        assert_eq!(
            decode_dynamic_texts(frame.message_id, &frame.payload, 4, 3).unwrap(),
            [
                "http://services.invalid/directory",
                "Directory",
                "extension"
            ]
        );
        assert_eq!(
            ServerMessage::decode(frame, ProtocolVersion::V19).unwrap(),
            from
        );
    }

    #[test]
    fn dynamic_7961_line_status_has_cisco_word_padding() {
        let bytes = ServerMessage::LineStatus {
            instance: 1,
            directory_number: "1006".into(),
            fully_qualified_display_name: "1006".into(),
            display_label: "1006".into(),
        }
        .encode(ProtocolVersion::V22)
        .unwrap();
        assert_eq!(bytes.len(), 36);
        assert_eq!(&bytes[..4], &28_u32.to_le_bytes());
        assert_eq!(
            &bytes[12..],
            b"\x01\0\0\0\x0f\0\0\x001006\x001006\x001006\0\0"
        );
    }

    #[test]
    fn dynamic_station_decoders_reject_missing_or_nonzero_word_padding() {
        let bytes = ServerMessage::LineStatus {
            instance: 1,
            directory_number: "1006".into(),
            fully_qualified_display_name: "1006".into(),
            display_label: "1006".into(),
        }
        .encode(ProtocolVersion::V22)
        .unwrap();
        let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);

        let mut missing_padding = frame.clone();
        missing_padding.payload.pop();
        assert!(matches!(
            ServerMessage::decode(missing_padding, ProtocolVersion::V22),
            Err(CodecError::InvalidAlignment { actual: 23, .. })
        ));

        let mut nonzero_padding = frame.clone();
        *nonzero_padding.payload.last_mut().unwrap() = 0x7f;
        assert!(matches!(
            ServerMessage::decode(nonzero_padding, ProtocolVersion::V22),
            Err(CodecError::TrailingBytes { count: 1, .. })
        ));

        let mut extension = frame;
        extension.payload.extend_from_slice(&[0; 4]);
        assert!(matches!(
            ServerMessage::decode(extension, ProtocolVersion::V22),
            Err(CodecError::TrailingBytes { count: 5, .. })
        ));
    }

    #[test]
    fn dynamic_display_decoders_validate_the_same_padding_contract() {
        let bytes = ServerMessage::DisplayNotify {
            timeout_seconds: 4,
            text: "status".into(),
        }
        .encode(ProtocolVersion::V22)
        .unwrap();
        let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
        assert_eq!(frame.payload.len() % 4, 0);

        let mut bad = frame;
        bad.payload.extend_from_slice(&[0, 0, 0, 1]);
        assert!(matches!(
            ServerMessage::decode(bad, ProtocolVersion::V22),
            Err(CodecError::TrailingBytes { .. })
        ));
    }

    #[test]
    fn legacy_station_labels_use_the_configured_single_byte_code_page() {
        let message = ServerMessage::LineStatus {
            instance: 1,
            directory_number: "1001".into(),
            fully_qualified_display_name: "Desk 1001".into(),
            display_label: "Räksmörgås".into(),
        };
        let latin1 = message
            .encode_for_legacy_station(ProtocolVersion::V3, LegacyCodePage::Iso8859_1)
            .unwrap();
        let latin1 = FrameDecoder::new().push(&latin1).unwrap().remove(0);
        let expected = b"R\xe4ksm\xf6rg\xe5s";
        assert!(
            latin1
                .payload
                .windows(expected.len())
                .any(|bytes| bytes == expected)
        );

        let ascii = message
            .encode_for_legacy_station(ProtocolVersion::V3, LegacyCodePage::Ascii)
            .unwrap();
        let ascii = FrameDecoder::new().push(&ascii).unwrap().remove(0);
        assert!(
            ascii
                .payload
                .windows(10)
                .any(|bytes| bytes == b"R?ksm?rg?s")
        );

        let utf8 = message.encode(ProtocolVersion::V3).unwrap();
        let utf8 = FrameDecoder::new().push(&utf8).unwrap().remove(0);
        assert!(
            utf8.payload
                .windows(13)
                .any(|bytes| bytes == "Räksmörgås".as_bytes())
        );
    }

    #[test]
    fn dynamic_station_statuses_support_extended_labels_and_require_terminators() {
        let label = "A label that is intentionally longer than the static forty-byte field";
        for message in [
            ServerMessage::LineStatus {
                instance: 1,
                directory_number: "1001".into(),
                fully_qualified_display_name: "Desk 1001".into(),
                display_label: label.into(),
            },
            ServerMessage::ServiceUrlStatus {
                index: 3,
                url: "http://services.invalid/directory".into(),
                label: label.into(),
                extension_text: String::new(),
            },
        ] {
            let bytes = message.encode(ProtocolVersion::V17).unwrap();
            let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
            assert_eq!(
                ServerMessage::decode(frame, ProtocolVersion::V17).unwrap(),
                message
            );
        }

        let feature = ServerMessage::FeatureStatus {
            instance: 2,
            button_type: ButtonType::DoNotDisturb,
            label: label.into(),
            state: 1,
        };
        let session =
            StationSessionContext::new(ProtocolVersion::V8, PhoneFeatures::DYNAMIC_MESSAGES);
        let bytes = feature.encode_for_session(session).unwrap();
        let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
        assert_eq!(frame.message_id, wire_id::FEATURE_STAT_DYNAMIC);
        assert_eq!(
            ServerMessage::decode(frame, ProtocolVersion::V8).unwrap(),
            feature
        );

        let unterminated_service = Frame::new(
            ProtocolVersion::V17.wire(),
            wire_id::SERVICE_URL_STAT_DYNAMIC,
            [3_u32.to_le_bytes().as_slice(), b"x\0YZ"].concat(),
        );
        assert!(matches!(
            ServerMessage::decode(unterminated_service, ProtocolVersion::V17),
            Err(CodecError::Truncated { .. })
        ));
    }

    #[test]
    fn call_info_layouts_preserve_redirecting_and_presentation_fields() {
        let info = CallInfo {
            direction: crate::types::CallDirection::Inbound,
            calling_name: "Festival Caller".into(),
            calling_number: "1001".into(),
            called_name: "Festival Phone".into(),
            called_number: "1006".into(),
            original_called_name: "Reception".into(),
            original_called_number: "1000".into(),
            last_redirecting_name: "Front Desk".into(),
            last_redirecting_number: "1002".into(),
            original_redirect_reason: 4,
            last_redirect_reason: 2,
            party_restrictions: 0xf,
        };
        for (protocol, expected_id) in [
            (ProtocolVersion::V3, wire_id::CALL_INFO),
            (ProtocolVersion::V8, wire_id::CALL_INFO),
            (ProtocolVersion::V16, wire_id::CALL_INFO_DYNAMIC),
            (ProtocolVersion::V22, wire_id::CALL_INFO_DYNAMIC),
        ] {
            let message = ServerMessage::CallInfo {
                info: info.clone(),
                line_instance: 1,
                call_reference: 42,
            };
            let bytes = message.encode(protocol).unwrap();
            assert_eq!(bytes.len() % 4, 0, "message 0x{expected_id:04x}");
            let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
            assert_eq!(frame.message_id, expected_id);
            assert_eq!(ServerMessage::decode(frame, protocol).unwrap(), message);
        }

        let bytes = ServerMessage::DisplayPrompt {
            timeout_seconds: 0,
            text: "From Festival Caller (1001)".into(),
            line_instance: 1,
            call_reference: 42,
        }
        .encode(ProtocolVersion::V22)
        .unwrap();
        let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
        assert_eq!(frame.message_id, wire_id::DISPLAY_DYNAMIC_PROMPT_STATUS);
        assert_eq!(
            ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
            ServerMessage::DisplayPrompt {
                timeout_seconds: 0,
                text: "From Festival Caller (1001)".into(),
                line_instance: 1,
                call_reference: 42,
            }
        );
    }

    #[test]
    fn notification_frames_select_static_or_dynamic_layout_and_keep_priority_six() {
        for (protocol, expected_id) in [
            (ProtocolVersion::V3, wire_id::DISPLAY_PRIORITY_NOTIFY),
            (
                ProtocolVersion::V22,
                wire_id::DISPLAY_DYNAMIC_PRIORITY_NOTIFY,
            ),
        ] {
            let message = ServerMessage::DisplayPriorityNotify {
                timeout_seconds: 10,
                priority: NotificationPriority::Timed,
                text: "Status line".into(),
            };
            let bytes = message.encode(protocol).unwrap();
            let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
            assert_eq!(frame.message_id, expected_id);
            assert_eq!(ServerMessage::decode(frame, protocol).unwrap(), message);
        }

        let message = ServerMessage::DisplayNotify {
            timeout_seconds: 3,
            text: "Dynamic notification text longer than thirty-one bytes".into(),
        };
        let bytes = message.encode(ProtocolVersion::V22).unwrap();
        let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
        assert_eq!(frame.message_id, wire_id::DISPLAY_DYNAMIC_NOTIFY);
        assert_eq!(
            ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
            message
        );
    }

    #[test]
    fn call_state_uses_cisco_visibility_then_precedence_layout() {
        let bytes = ServerMessage::CallState {
            state: CallState::RingIn,
            line_instance: 1,
            call_reference: 42,
        }
        .encode(ProtocolVersion::V22)
        .unwrap();
        let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
        let words = (0..frame.payload.len() / 4)
            .map(|index| {
                let offset = index * 4;
                u32::from_le_bytes([
                    frame.payload[offset],
                    frame.payload[offset + 1],
                    frame.payload[offset + 2],
                    frame.payload[offset + 3],
                ])
            })
            .collect::<Vec<_>>();

        assert_eq!(
            words,
            vec![CallState::RingIn.wire_value(), 1, 42, 0, 2, 0],
            "CallState is state, line, call, visibility, priority, domain"
        );

        for (state, expected) in [
            (CallState::OffHook, 3),
            (CallState::Proceed, 3),
            (CallState::Connected, 3),
            (CallState::RingOut, 4),
        ] {
            let bytes = ServerMessage::CallState {
                state,
                line_instance: 1,
                call_reference: 42,
            }
            .encode(ProtocolVersion::V22)
            .unwrap();
            let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
            assert_eq!(
                u32::from_le_bytes(frame.payload[16..20].try_into().unwrap()),
                expected,
                "wrong precedence for {state:?}"
            );
        }
    }

    #[test]
    fn soft_key_sets_and_masks_only_advertise_implemented_actions() {
        let profile = SoftKeyProfile::default();
        let bytes = ServerMessage::SoftKeySet {
            profile: profile.clone(),
        }
        .encode(ProtocolVersion::V22)
        .unwrap();
        let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
        let payload: WireSoftKeySet = decode(frame.message_id, &frame.payload).unwrap();

        assert_eq!(
            &payload.sets[KeyMode::RingIn.wire_value() as usize].template_indexes[..2],
            &[
                SoftKey::Answer.wire_value() as u8,
                SoftKey::EndCall.wire_value() as u8
            ]
        );
        assert_eq!(profile.valid_mask(KeyMode::RingIn), 0b11);
        assert_eq!(profile.valid_mask(KeyMode::Connected), 0b111);
        assert_eq!(profile.valid_mask(KeyMode::Empty), 0);
    }

    #[test]
    fn configured_soft_key_set_round_trips_order_and_empty_modes() {
        let profile = SoftKeyProfile::new(KeyMode::ALL_KNOWN.iter().copied().map(|mode| {
            let actions = match mode {
                KeyMode::OnHook => vec![SoftKey::Redial, SoftKey::NewCall],
                KeyMode::Connected => vec![SoftKey::EndCall, SoftKey::Hold],
                _ => Vec::new(),
            };
            (mode, actions)
        }))
        .unwrap();
        let message = ServerMessage::SoftKeySet {
            profile: profile.clone(),
        };
        let bytes = message.encode(ProtocolVersion::V22).unwrap();
        let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
        let payload: WireSoftKeySet = decode(frame.message_id, &frame.payload).unwrap();

        assert_eq!(
            &payload.sets[KeyMode::OnHook.wire_value() as usize].template_indexes[..3],
            &[
                SoftKey::Redial.wire_value() as u8,
                SoftKey::NewCall.wire_value() as u8,
                0,
            ]
        );
        assert_eq!(
            &payload.sets[KeyMode::Connected.wire_value() as usize].template_indexes[..3],
            &[
                SoftKey::EndCall.wire_value() as u8,
                SoftKey::Hold.wire_value() as u8,
                0,
            ]
        );
        assert_eq!(
            ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
            message
        );
        assert_eq!(profile.valid_mask(KeyMode::OnHook), 0b11);
        assert_eq!(profile.valid_mask(KeyMode::RingIn), 0);

        let template = ServerMessage::SoftKeyTemplate {
            actions: profile.template_actions(),
        };
        let bytes = template.encode(ProtocolVersion::V22).unwrap();
        let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
        let payload: WireSoftKeyTemplate = decode(frame.message_id, &frame.payload).unwrap();
        assert_eq!(payload.definitions[0].event, SoftKey::Redial.wire_value());
        assert_eq!(payload.definitions[2].event, SoftKey::Hold.wire_value());
        assert_eq!(payload.definitions[3].event, 0);
        assert_eq!(
            ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
            template
        );
    }

    #[test]
    fn nominally_empty_requests_accept_bounded_extensions() {
        for message_id in [
            wire_id::CONFIG_STAT_REQ,
            wire_id::TIME_DATE_REQ,
            wire_id::VERSION_REQ,
            wire_id::SERVER_REQ,
            wire_id::SOFT_KEY_SET_REQ,
            wire_id::SOFT_KEY_TEMPLATE_REQ,
        ] {
            ClientMessage::decode(Frame::new(22, message_id, 34_u32.to_le_bytes().to_vec()))
                .unwrap();
        }
    }

    #[test]
    fn dtmf_payload_messages_use_their_structural_word_layouts() {
        let identity = DtmfPayloadIdentity {
            payload_type: 101,
            conference_id: 0x1122_3344,
            passthrough_party_id: 0x5566_7788,
        };
        let request = DtmfPayloadRequest {
            payload_type: identity.payload_type,
            conference_id: identity.conference_id,
            passthrough_party_id: identity.passthrough_party_id,
            dtmf_type: 2,
        };
        let identity_payload = [
            identity.payload_type.to_le_bytes(),
            identity.conference_id.to_le_bytes(),
            identity.passthrough_party_id.to_le_bytes(),
        ]
        .concat();
        let request_payload = [
            request.payload_type.to_le_bytes(),
            request.conference_id.to_le_bytes(),
            request.passthrough_party_id.to_le_bytes(),
            request.dtmf_type.to_le_bytes(),
        ]
        .concat();

        for message in [
            ClientMessage::SubscribeDtmfPayloadResponse(identity),
            ClientMessage::UnsubscribeDtmfPayloadResponse(identity),
        ] {
            let frame = FrameDecoder::new()
                .push(&message.encode(ProtocolVersion::V22).unwrap())
                .unwrap()
                .remove(0);
            assert_eq!(frame.payload, identity_payload);
            assert_eq!(
                ClientMessage::decode_with_version(frame, ProtocolVersion::V22).unwrap(),
                message
            );
        }

        for (message, expected_payload) in [
            (
                ServerMessage::SubscribeDtmfPayloadRequest(request),
                request_payload.as_slice(),
            ),
            (
                ServerMessage::SubscribeDtmfPayloadError(identity),
                identity_payload.as_slice(),
            ),
            (
                ServerMessage::UnsubscribeDtmfPayloadRequest(request),
                request_payload.as_slice(),
            ),
            (
                ServerMessage::UnsubscribeDtmfPayloadError(identity),
                identity_payload.as_slice(),
            ),
        ] {
            let frame = FrameDecoder::new()
                .push(&message.encode(ProtocolVersion::V22).unwrap())
                .unwrap()
                .remove(0);
            assert_eq!(frame.payload.as_slice(), expected_payload);
            assert_eq!(
                ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
                message
            );
        }

        assert!(
            ClientMessage::decode_with_version(
                Frame::new(22, wire_id::SUBSCRIBE_DTMF_PAYLOAD_RES, vec![0; 13]),
                ProtocolVersion::V22,
            )
            .is_err()
        );
        assert!(
            ServerMessage::decode(
                Frame::new(22, wire_id::SUBSCRIBE_DTMF_PAYLOAD_REQ, vec![0; 17]),
                ProtocolVersion::V22,
            )
            .is_err()
        );
    }

    #[test]
    fn add_participant_response_preserves_progressive_identifier_bytes() {
        for identifier_len in [0, 1, 64, 256] {
            let identifier = (0..identifier_len)
                .map(|index| (index as u8).wrapping_mul(17).wrapping_add(3))
                .collect::<Vec<_>>();
            let mut payload = [
                42_u32.to_le_bytes(),
                100_u32.to_le_bytes(),
                0_u32.to_le_bytes(),
            ]
            .concat();
            payload.extend_from_slice(&identifier);
            let decoded = ControlMessage::decode(
                Frame::new(22, wire_id::ADD_PARTICIPANT_RES, payload),
                ProtocolVersion::V22,
            )
            .unwrap();
            let ControlMessage::AddParticipantResponse(response) = &decoded else {
                panic!("expected add-participant response");
            };
            assert_eq!(response.bridge_participant_id.as_bytes(), identifier);
            let frame = FrameDecoder::new()
                .push(&decoded.encode(ProtocolVersion::V22).unwrap())
                .unwrap()
                .remove(0);
            assert_eq!(frame.payload.len(), 272);
            assert_eq!(&frame.payload[12..12 + identifier_len], identifier);
            assert!(
                frame.payload[12 + identifier_len..]
                    .iter()
                    .all(|byte| *byte == 0)
            );
        }

        let identifier = (0..257)
            .map(|index| (index as u8).wrapping_mul(17).wrapping_add(3))
            .collect::<Vec<_>>();
        let canonical = ControlMessage::AddParticipantResponse(AddParticipantResponse {
            conference_id: 42.into(),
            call_reference: 100.into(),
            result: AddParticipantResult::Ok,
            bridge_participant_id: BoundedBytes::try_from(identifier).unwrap(),
        });
        let frame = FrameDecoder::new()
            .push(&canonical.encode(ProtocolVersion::V22).unwrap())
            .unwrap()
            .remove(0);
        assert_eq!(frame.payload.len(), 272);
        assert_eq!(
            ControlMessage::decode(frame, ProtocolVersion::V22).unwrap(),
            canonical
        );

        for invalid_len in [270, 271, 273] {
            assert!(
                ControlMessage::decode(
                    Frame::new(22, wire_id::ADD_PARTICIPANT_RES, vec![0; invalid_len]),
                    ProtocolVersion::V22,
                )
                .is_err()
            );
        }
        let mut invalid_alignment = vec![0; 272];
        invalid_alignment[271] = 1;
        assert!(
            ControlMessage::decode(
                Frame::new(22, wire_id::ADD_PARTICIPANT_RES, invalid_alignment),
                ProtocolVersion::V22,
            )
            .is_err()
        );
    }

    #[test]
    fn xml_alarm_accepts_and_preserves_every_bounded_frame_form() {
        for payload_len in [0, 1, 2_000, 2_004, 2_048] {
            let payload = (0..payload_len)
                .map(|index| (index as u8).wrapping_mul(29).wrapping_add(1))
                .collect::<Vec<_>>();
            let decoded = ClientMessage::decode_with_version(
                Frame::new(22, wire_id::XML_ALARM, payload.clone()),
                ProtocolVersion::V22,
            )
            .unwrap();
            let ClientMessage::XmlAlarm(message) = &decoded else {
                panic!("expected XML alarm");
            };
            assert_eq!(message.wire_payload(), payload.as_slice());
            let frame = FrameDecoder::new()
                .push(&decoded.encode(ProtocolVersion::V22).unwrap())
                .unwrap()
                .remove(0);
            assert_eq!(frame.payload, payload);
        }

        assert!(matches!(
            ClientMessage::decode_with_version(
                Frame::new(22, wire_id::XML_ALARM, vec![0; 2_049]),
                ProtocolVersion::V22,
            ),
            Err(CodecError::CountTooLarge {
                message_id: wire_id::XML_ALARM,
                count: 2_049,
                maximum: 2_048,
                ..
            })
        ));

        let with_suffix =
            XmlAlarmMessage::from_wire_payload(b"<alarm/>\0ignored".to_vec()).unwrap();
        assert_eq!(with_suffix.xml_bytes(), b"<alarm/>");
        assert_eq!(with_suffix.wire_payload(), b"<alarm/>\0ignored");

        let canonical = XmlAlarmMessage::from_xml(vec![b'x'; 2_000]).unwrap();
        assert_eq!(canonical.xml_bytes().len(), 2_000);
        assert_eq!(canonical.wire_payload().len(), 2_004);
        assert!(XmlAlarmMessage::from_xml(vec![b'x'; 2_001]).is_err());
    }

    #[test]
    fn xml_alarm_preserves_bounded_wire_payload() {
        let xml = "<?xml version=\"1.0\"?><x-cisco-alarm></x-cisco-alarm>";
        let mut payload = vec![0; 2_000];
        payload[..xml.len()].copy_from_slice(xml.as_bytes());

        let decoded =
            ClientMessage::decode(Frame::new(0, wire_id::XML_ALARM, payload.clone())).unwrap();
        let ClientMessage::XmlAlarm(message) = &decoded else {
            panic!("expected XML alarm");
        };
        assert_eq!(message.xml_bytes(), xml.as_bytes());
        assert_eq!(message.wire_payload(), payload);
        let frame = FrameDecoder::new()
            .push(&decoded.encode(ProtocolVersion::V22).unwrap())
            .unwrap()
            .remove(0);
        assert_eq!(frame.payload, payload);
    }

    #[test]
    fn location_information_uses_text_storage_followed_by_zero_alignment() {
        let maximum = "x".repeat(2_400);
        let encoded = ClientMessage::LocationInfo {
            xml: maximum.clone(),
        }
        .encode(ProtocolVersion::V22)
        .unwrap();
        let frame = FrameDecoder::new().push(&encoded).unwrap().remove(0);
        assert_eq!(frame.payload.len(), 2_404);
        assert_eq!(&frame.payload[2_400..], &[0, 0, 0, 0]);
        assert_eq!(
            ClientMessage::decode_with_version(frame, ProtocolVersion::V22).unwrap(),
            ClientMessage::LocationInfo { xml: maximum }
        );

        assert!(matches!(
            ClientMessage::LocationInfo {
                xml: "x".repeat(2_401),
            }
            .encode(ProtocolVersion::V22),
            Err(CodecError::TextTooLong {
                message_id: wire_id::LOCATION_INFO,
                maximum: 2_400,
                ..
            })
        ));

        let mut nonzero_alignment = vec![0; 2_404];
        nonzero_alignment[2_401] = 1;
        assert!(matches!(
            ClientMessage::decode_with_version(
                Frame::new(22, wire_id::LOCATION_INFO, nonzero_alignment),
                ProtocolVersion::V22,
            ),
            Err(CodecError::InvalidValue {
                message_id: wire_id::LOCATION_INFO,
                field: "reserved payload byte",
                ..
            })
        ));
    }

    #[test]
    fn decodes_7961_button_template_request_with_payload() {
        assert_eq!(
            ClientMessage::decode(Frame::new(
                22,
                wire_id::BUTTON_TEMPLATE_REQ,
                34_u32.to_le_bytes().to_vec(),
            ))
            .unwrap(),
            ClientMessage::ButtonTemplateRequest
        );
    }
}
