//! Parsing, normalization, and validation for `sccp.conf`.
//!
//! [`ModuleConfig::parse`] accepts `[general]`, device, line, and soft-key
//! profile sections plus Asterisk-style templates. It resolves inheritance,
//! ordered repeated fields, button positions, codec operations, line
//! appearances, feature policy, network/listener policy, channel metadata,
//! registration targets, and referenced profiles into one immutable snapshot.
//! Unknown keys, wrong-scope keys, conflicting aliases, duplicate scalar
//! settings, invalid references, ambiguous shared targets, and out-of-bound text
//! reject the complete candidate.
//!
//! The installed module reads `sccp.conf` under Asterisk's compiled
//! configuration directory unless the `SCCP_CONFIG` environment variable names
//! an exact path. [`provider`] supplies file, realtime, and hybrid candidates;
//! [`realtime`] preserves backend ordering and `NULL`/empty distinctions; and
//! the feature-gated `reload` module plans transactional application without
//! weakening parser rules.
//!
//! The repository's `sccp.conf.example` uses one canonical spelling for every
//! supported semantic option. Compatibility aliases are parser inputs, not
//! additional settings, so mutually exclusive aliases are described in
//! owning type/field documentation instead of being combined in the sample.
//! Parser tests load that distributed sample and assert representative values
//! at general, device, line, button, media, feature, registration, date/time,
//! and MWI scopes.
//!
//! # Network policy boundary
//!
//! The runtime binds the configured clear listener and optional secure
//! listener. Per-device transport requirements are enforced at registration.
//! NAT mode, `localnet`, external/advertised IPv4 and IPv6 addresses, and
//! hostname refresh are active inputs to signaling-peer and RTP address
//! selection. Address-family mismatch, unusable endpoints, and unresolved
//! required external addresses fail closed.
//!
//! Sensitive configuration values use typed wrappers with redacted [`Debug`]
//! output. In particular, mobility PIN comparison scans the full fixed seven
//! digit bound without a data-dependent early exit; PINs, forwarding targets,
//! channel-variable values, TLS paths where marked sensitive, and opaque
//! provider values do not appear in validation diagnostics.

mod canonical;
pub mod convergence;
mod defaults;
mod inheritance;
mod model;
mod parsing;
pub use model::*;
pub mod provider;
pub mod realtime;
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
pub mod reload;
mod section_values;
mod serde_section;
pub mod sorcery;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use sccp_protocol::{
    AddonModuleDefinition, AppearanceRingMode, AudioProcessingPolicy, BlfSpeedDialDefinition,
    ButtonDefinition, ButtonType, Codec, CodecKind, DateTemplate, DeviceDefinition, DeviceId,
    DeviceType, DtmfMode, EchoCancellation, FeatureDefinition, KeyMode, LampMode, LegacyCodePage,
    LineAppearance, LineDefinition, RecordingButtonDefinition, RingerMode, ServiceDefinition,
    SignalingQos, SignalingServerRoute, SilenceSuppression, SoftKey,
    SoftKeyProfile as StationSoftKeyProfile, SpeedDialDefinition, StationTransportRequirement,
    StationUiPolicy, Tone,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::call::forwarding::ForwardingDestination;
use crate::call::hotline::{HotlineDestination, MAX_HOTLINE_DESTINATION_BYTES};
use crate::call::metadata::{
    ChannelVariable, MAX_ACCOUNT_CODE_BYTES, MAX_LANGUAGE_BYTES, MAX_VARIABLE_AGGREGATE_BYTES,
    MAX_VARIABLES,
};
use crate::call::voicemail::VoicemailDestination;
use crate::media::encryption::{
    MediaEncryptionPolicy, MediaEncryptionProfile, MediaEncryptionRequirement,
};
use crate::media::formats::{pbx_audio_format, unsupported_audio_reason};
use canonical::{
    canonical_section_entries, canonical_section_rank, check_canonical_section, source_section_kind,
};
use inheritance::{TemplateKind, resolve_inheritance};
use section_values::SectionValues;
use serde_section::{deserialize_entries, deserialize_section, serialized_key};

pub const DEFAULT_SOFT_KEY_PROFILE: &str = "default";
pub const MAX_SOFT_KEYS_PER_MODE: usize = 16;
pub const MAX_CODEC_PREFERENCES: usize = 32;
pub const MAX_HOTLINE_FIELD_BYTES: usize = MAX_HOTLINE_DESTINATION_BYTES;
pub const MAX_EXTERNAL_REFRESH_SECONDS: u32 = 86_400;
pub const MAX_REALTIME_FAMILY_BYTES: usize = 45;
pub const MAX_MOBILITY_PIN_DIGITS: usize = 7;
pub const MAX_REGISTRATION_IDENTIFIER_BYTES: usize = 79;
pub const MAX_REGISTRATION_EXTENSION_LIST_BYTES: usize = 255;

const KEY_MODES: [KeyMode; 14] = [
    KeyMode::OnHook,
    KeyMode::Connected,
    KeyMode::OnHold,
    KeyMode::RingIn,
    KeyMode::OffHook,
    KeyMode::ConnectedTransfer,
    KeyMode::DigitsFollowing,
    KeyMode::ConnectedConference,
    KeyMode::RingOut,
    KeyMode::OffHookFeature,
    KeyMode::InUseHint,
    KeyMode::OnHookStealable,
    KeyMode::HoldConference,
    KeyMode::Empty,
];

impl GeneralConfig {
    pub fn timing_policy(&self) -> GeneralTimingPolicy {
        GeneralTimingPolicy {
            keepalive: Duration::from_secs(self.keepalive_seconds.into()),
            secondary_keepalive: Duration::from_secs(self.secondary_keepalive_seconds.into()),
            first_digit_timeout: Duration::from_millis(self.first_digit_timeout_ms),
            interdigit_timeout: Duration::from_millis(self.interdigit_timeout_ms),
            call_waiting_repeat: Duration::from_secs(self.call_waiting_interval_seconds.into()),
        }
    }

    pub fn station_policy(&self) -> GeneralStationPolicy {
        GeneralStationPolicy {
            timezone_offset_minutes: self.timezone_offset_minutes,
            date_template: self.date_template.clone(),
            ring_type: self.ring_type,
            call_waiting_tone: self.call_waiting_tone,
        }
    }
}

impl fmt::Debug for GeneralConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GeneralConfig")
            .field("configuration_source", &self.configuration_source)
            .field("bind", &self.bind)
            .field("advertised_address", &self.advertised_address)
            .field("server_name", &self.server_name)
            .field("language", &self.language)
            .field(
                "account_code",
                &self.account_code.as_ref().map(|_| "<redacted>"),
            )
            .field("keepalive_seconds", &self.keepalive_seconds)
            .field(
                "secondary_keepalive_seconds",
                &self.secondary_keepalive_seconds,
            )
            .field("signaling_servers", &self.signaling_servers)
            .field("codecs", &self.codecs)
            .field("audio_encryption", &self.audio_encryption)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for TlsCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CombinedPem(_) => formatter.write_str("CombinedPem(<redacted>)"),
            Self::SplitPem { trust_store, .. } => formatter
                .debug_struct("SplitPem")
                .field("certificate", &"<redacted>")
                .field("private_key", &"<redacted>")
                .field("trust_store", &trust_store.as_ref().map(|_| "<redacted>"))
                .finish(),
        }
    }
}

impl fmt::Debug for TlsListener {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TlsListener")
            .field("bind", &self.bind)
            .field("credentials", &self.credentials)
            .finish()
    }
}

impl From<TransportRequirement> for StationTransportRequirement {
    fn from(requirement: TransportRequirement) -> Self {
        match requirement {
            TransportRequirement::Clear => Self::Clear,
            TransportRequirement::Tls => Self::Secure,
            TransportRequirement::Either => Self::Either,
        }
    }
}

impl fmt::Debug for MobilityPin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MobilityPin(<redacted>)")
    }
}

impl fmt::Display for HintTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}@{}", self.extension, self.context)
    }
}

impl fmt::Debug for LineConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LineConfig")
            .field("number", &self.number)
            .field("label", &self.label)
            .field("context", &self.context)
            .field("caller_name", &self.caller_name)
            .field("caller_number", &self.caller_number)
            .field("mailbox", &self.mailbox)
            .field("language", &self.language)
            .field(
                "account_code",
                &self.account_code.as_ref().map(|_| "<redacted>"),
            )
            .field("channel_variables", &self.channel_variables)
            .finish()
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ConfigError {
    #[error("line {line}: {message}")]
    Syntax { line: usize, message: String },
    #[error("section [{section}] has an unknown type {kind}")]
    UnknownSectionType { section: String, kind: String },
    #[error("section [{0}] is missing type=device, type=line, or type=softkey_profile")]
    MissingSectionType(String),
    #[error("duplicate section [{0}]")]
    DuplicateSection(String),
    #[error("{key}: invalid value {value}")]
    InvalidValue { key: String, value: String },
    #[error("device {device} references unknown line {line}")]
    UnknownLine { device: DeviceId, line: String },
    #[error("device {device} references unknown soft-key profile {profile}")]
    UnknownSoftKeyProfile { device: DeviceId, profile: String },
    #[error("section [{section}] references missing template [{parent}]")]
    MissingTemplate { section: String, parent: String },
    #[error("section [{section}] references non-template section [{parent}]")]
    ParentIsNotTemplate { section: String, parent: String },
    #[error(
        "section [{section}] is type={child_kind} but template [{parent}] is type={parent_kind}"
    )]
    WrongTemplateKind {
        section: String,
        child_kind: String,
        parent: String,
        parent_kind: String,
    },
    #[error("template [{section}] must resolve to type=device or type=line, got {kind}")]
    InvalidTemplateKind { section: String, kind: String },
    #[error("inheritance cycle: {0}")]
    InheritanceCycle(String),
    #[error("line {0} is not assigned to a device")]
    UnassignedLine(String),
    #[error("device {0} has no lines")]
    DeviceWithoutLines(DeviceId),
    #[error("configuration must contain at least one device and one line")]
    Empty,
    #[error("invalid SCCP device ID: {0}")]
    InvalidDevice(String),
}

#[derive(Clone, Default)]
struct RawSection {
    name: String,
    line: usize,
    is_template: bool,
    parents: Vec<String>,
    values: Vec<RawValue>,
}

#[derive(Clone)]
struct RawValue {
    key: String,
    value: String,
    line: usize,
    section: String,
}

/// Serde is the authoritative spelling table for general options. Aliases are
/// accepted production inputs; serialization always yields the canonical
/// Asterisk-style spelling used by examples and diagnostics.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum GeneralOption {
    /// Selects whether configuration comes from files or a realtime backend.
    /// Controls which loading and reload path supplies SCCP objects.
    ConfigurationSource,
    #[serde(rename = "dateformat")]
    /// Sets the date template sent to registered stations.
    /// Controls how phones render the server-provided calendar date.
    DateFormat,
    #[serde(rename = "tzoffset")]
    /// Sets the station clock offset from server time.
    /// Applies the configured offset in minutes to SCCP time updates.
    TimezoneOffset,
    #[serde(alias = "clearbind")]
    /// Sets the combined SCCP listener endpoint.
    /// Configures the address and port used for unencrypted signaling.
    Bind,
    #[serde(alias = "bindaddr", alias = "clearbindaddr")]
    /// Sets the address of the unencrypted SCCP listener.
    /// Combines with the configured port to form the listener endpoint.
    BindAddress,
    #[serde(alias = "clearport")]
    /// Sets the port of the unencrypted SCCP listener.
    /// Combines with the configured bind address to form the endpoint.
    Port,
    /// Sets the legacy signaling address advertised to stations.
    /// Provides an externally reachable address when it differs from the bind address.
    AdvertisedAddress,
    #[serde(alias = "advertisedaddressipv4")]
    /// Sets the IPv4 signaling address advertised to stations.
    /// Provides the station-facing IPv4 endpoint behind routing or NAT.
    AdvertisedIpv4,
    #[serde(alias = "advertisedaddressipv6")]
    /// Sets the IPv6 signaling address advertised to stations.
    /// Provides the station-facing IPv6 endpoint when available.
    AdvertisedIpv6,
    #[serde(alias = "securebind")]
    /// Sets the combined TLS SCCP listener endpoint.
    /// Configures the address and port used for encrypted signaling.
    TlsBind,
    #[serde(alias = "secbindaddr", alias = "tlsbindaddr")]
    /// Sets the address of the TLS SCCP listener.
    /// Combines with the TLS port to form the encrypted endpoint.
    TlsBindAddress,
    #[serde(alias = "secport", alias = "tlsport")]
    /// Sets the port of the TLS SCCP listener.
    /// Combines with the TLS bind address to form the encrypted endpoint.
    TlsPort,
    #[serde(alias = "certfile", alias = "tlscombinedpem")]
    /// Sets a PEM file containing the TLS identity material.
    /// Supplies the certificate and private key for encrypted SCCP sessions.
    TlsCombinedPem,
    #[serde(alias = "tlscertificatefile")]
    /// Sets the TLS server certificate file.
    /// Presents this certificate to stations during the TLS handshake.
    TlsCertificate,
    #[serde(alias = "tlsprivatekeyfile")]
    /// Sets the TLS server private-key file.
    /// Uses this key with the configured certificate for TLS sessions.
    TlsPrivateKey,
    #[serde(alias = "tlscafile")]
    /// Sets the trusted certificate-authority file.
    /// Controls certificate trust for authenticated TLS connections.
    TlsTrustStore,
    /// Adds a source network to the general deny ACL.
    /// Blocks matching station signaling connections unless a later rule permits them.
    Deny,
    /// Adds a source network to the general permit ACL.
    /// Allows matching station signaling connections under the configured ACL.
    Permit,
    #[serde(rename = "localnet")]
    /// Declares an address range as locally routed.
    /// Guides external-address and NAT decisions for station and media endpoints.
    LocalNetwork,
    #[serde(rename = "externip", alias = "externaladdress")]
    /// Sets the fixed external address for NAT traversal.
    /// Advertises this address to peers outside configured local networks.
    ExternalAddress,
    #[serde(rename = "externhost", alias = "externalhost")]
    /// Sets a hostname used to discover the external address.
    /// Supports deployments whose public address changes over time.
    ExternalHost,
    #[serde(rename = "externrefresh", alias = "externalrefresh")]
    /// Sets how often the external hostname is resolved.
    /// Refreshes dynamic public addressing at the configured interval.
    ExternalRefresh,
    /// Sets the default NAT handling mode.
    /// Controls address rewriting for station signaling and media endpoints.
    Nat,
    #[serde(rename = "sccp_tos", alias = "signalingtos")]
    /// Sets the IP type-of-service byte for SCCP signaling.
    /// Applies the traffic classification to signaling sockets.
    SignalingTos,
    #[serde(
        rename = "sccp_dscp",
        alias = "sccpdscp",
        alias = "signalingdscp",
        alias = "signaling_dscp"
    )]
    /// Sets the DSCP value for SCCP signaling.
    /// Encodes the value into the signaling traffic-class byte.
    SignalingDscp,
    #[serde(rename = "sccp_cos", alias = "signalingcos", alias = "signaling_cos")]
    /// Sets the layer-two class of service for SCCP signaling.
    /// Applies the priority to supported signaling interfaces.
    SignalingCos,
    #[serde(alias = "audiotos")]
    /// Sets the default IP type-of-service byte for audio.
    /// Applies the traffic classification to station audio streams.
    AudioTos,
    #[serde(alias = "audiodscp")]
    /// Sets the default DSCP value for audio.
    /// Encodes the value into audio media traffic classes.
    AudioDscp,
    #[serde(alias = "audiocos")]
    /// Sets the default layer-two class of service for audio.
    /// Applies the priority to supported audio interfaces.
    AudioCos,
    #[serde(alias = "videotos")]
    /// Sets the default IP type-of-service byte for video.
    /// Applies the traffic classification to station video streams.
    VideoTos,
    #[serde(alias = "videodscp")]
    /// Sets the default DSCP value for video.
    /// Encodes the value into video media traffic classes.
    VideoDscp,
    #[serde(alias = "videocos")]
    /// Sets the default layer-two class of service for video.
    /// Applies the priority to supported video interfaces.
    VideoCos,
    #[serde(alias = "trustphoneip")]
    /// Controls whether station-reported IP addresses are trusted.
    /// Determines whether media uses reported addresses or the signaling peer address.
    TrustPhoneIp,
    #[serde(alias = "servername")]
    /// Sets the signaling-server name presented to stations.
    /// Identifies this SCCP server in provisioning responses.
    ServerName,
    /// Sets the default Asterisk language for SCCP calls.
    /// Provides the inherited language for devices and lines.
    Language,
    #[serde(rename = "accountcode")]
    /// Sets the default Asterisk account code for SCCP calls.
    /// Provides the inherited billing or CDR account identifier.
    AccountCode,
    /// Sets the primary station keepalive interval.
    /// Controls the normal signaling liveness cadence.
    Keepalive,
    /// Sets the secondary station keepalive interval.
    /// Controls the alternate liveness cadence advertised during registration.
    SecondaryKeepalive,
    /// Adds a signaling-server endpoint advertised to stations.
    /// Builds the primary and failover server list returned during provisioning.
    SignalingServer,
    #[serde(alias = "firstdigittimeout")]
    /// Sets how long dialing waits for the first digit.
    /// Controls collection timeout before any called-party digit arrives.
    FirstDigitTimeout,
    /// Sets the millisecond timeout between dialed digits.
    /// Controls when digit collection treats an entered number as complete.
    InterdigitTimeoutMs,
    #[serde(alias = "digittimeout")]
    /// Sets the normal timeout between dialed digits.
    /// Provides the legacy-duration form of the interdigit timer.
    DigitTimeout,
    #[serde(alias = "digittimeoutchar")]
    /// Sets the character that immediately completes digit collection.
    /// Lets callers bypass the remaining interdigit timeout.
    DigitTimeoutChar,
    #[serde(alias = "recorddigittimeoutchar")]
    /// Controls whether the digit-completion character is retained.
    /// Determines whether the terminator becomes part of the collected number.
    RecordDigitTimeoutChar,
    /// Controls whether digit collection is presented as en-bloc dialing.
    /// Defers call routing until the complete called number is available.
    SimulateEnbloc,
    #[serde(alias = "speeddialawaitfurtherdigits")]
    /// Controls whether speed dials accept additional digits.
    /// Keeps digit collection open after a speed-dial value is inserted.
    SpeedDialAwaitFurtherDigits,
    #[serde(alias = "allowoverlap")]
    /// Controls overlap dialing by default.
    /// Allows routing to begin before the full destination is known.
    AllowOverlap,
    /// Controls whether hanging up completes an attended transfer.
    /// Applies the transfer completion behavior to SCCP calls.
    TransferOnHangup,
    #[serde(alias = "callanswerorder")]
    /// Sets which ringing call is answered first.
    /// Controls selection when a station has multiple answerable calls.
    CallAnswerOrder,
    #[serde(alias = "ringtype")]
    /// Sets the default station ring pattern.
    /// Selects the ringer behavior used for incoming calls.
    RingType,
    #[serde(alias = "callwaitingtone")]
    /// Sets the tone used for a waiting call.
    /// Controls the in-call audible notification of another incoming call.
    CallWaitingTone,
    #[serde(alias = "callwaitinginterval")]
    /// Sets how often the call-waiting tone repeats.
    /// Controls the reminder cadence while another call waits.
    CallWaitingInterval,
    /// Marks a signaling server as a fallback endpoint.
    /// Influences how stations order and use advertised servers.
    Fallback,
    /// Sets the registration retry delay for a signaling server.
    /// Controls how long stations wait after a rejected token request.
    BackoffTime,
    /// Sets the advertised priority of a signaling server.
    /// Orders primary and failover endpoints presented to stations.
    ServerPriority,
    /// Adds codecs to the default allowed media set.
    /// Extends the codec policy inherited by SCCP lines and devices.
    Allow,
    /// Removes codecs from the default allowed media set.
    /// Restricts the codec policy inherited by SCCP lines and devices.
    Disallow,
    #[serde(rename = "meetme")]
    /// Enables the default conference application integration.
    /// Controls whether SCCP lines may invoke configured conferencing.
    ConferenceEnabled,
    #[serde(rename = "meetmeopts")]
    /// Sets default options passed to the conference application.
    /// Provides inherited conference behavior for SCCP calls.
    ConferenceOptions,
    #[serde(alias = "autoanswerringtime")]
    /// Sets how long an auto-answer call rings before answering.
    /// Controls the alerting delay for automatic answer.
    AutoanswerRingTime,
    #[serde(alias = "autoanswertone")]
    /// Sets the tone played when a call auto-answers.
    /// Provides an audible warning before the media path opens.
    AutoanswerTone,
    #[serde(alias = "remotehangup_tone")]
    /// Sets the tone played after the remote party hangs up.
    /// Controls station feedback when the far end clears a call.
    RemoteHangupTone,
    #[serde(alias = "hotlineenabled")]
    /// Enables the default hotline behavior.
    /// Controls whether eligible off-hook events immediately dial a destination.
    HotlineEnabled,
    #[serde(alias = "hotlineextension")]
    /// Sets the default hotline destination.
    /// Supplies the extension dialed by hotline-enabled appearances.
    HotlineExtension,
    #[serde(alias = "hotlinecontext")]
    /// Sets the dialplan context for the default hotline.
    /// Controls where the hotline extension is resolved.
    HotlineContext,
    #[serde(alias = "hotlinelabel")]
    /// Sets the display label for the default hotline.
    /// Provides station-facing text for the hotline appearance.
    HotlineLabel,
    #[serde(rename = "direct_media", alias = "directrtp")]
    /// Controls whether media may flow directly between endpoints.
    /// Avoids anchoring RTP at Asterisk when call conditions permit.
    DirectMedia,
    #[serde(rename = "early_media", alias = "earlyrtp")]
    /// Controls whether media opens before call answer.
    /// Allows progress audio or video during early call states.
    EarlyMedia,
    #[serde(alias = "audioencryption")]
    /// Sets the default audio-encryption policy.
    /// Controls whether SCCP audio channels negotiate encrypted media.
    AudioEncryption,
    #[serde(rename = "echocancel")]
    /// Sets the default station echo-cancellation mode.
    /// Applies the requested behavior when opening audio channels.
    EchoCancel,
    #[serde(rename = "silencesuppression")]
    /// Sets the default silence-suppression mode.
    /// Controls voice-activity transmission on station audio streams.
    SilenceSuppression,
    #[serde(alias = "jbenable")]
    /// Enables the Asterisk jitter buffer for SCCP media.
    /// Applies jitter smoothing to eligible received audio.
    JbEnable,
    #[serde(alias = "jbforce")]
    /// Forces jitter buffering even when normally unnecessary.
    /// Overrides automatic jitter-buffer activation decisions.
    JbForce,
    #[serde(alias = "jblog")]
    /// Enables jitter-buffer frame logging.
    /// Emits diagnostic details about buffered media frames.
    JbLog,
    #[serde(alias = "jbmaxsize")]
    /// Sets the maximum jitter-buffer size in milliseconds.
    /// Bounds how much received audio may be delayed for smoothing.
    JbMaxSize,
    #[serde(alias = "jbresyncthreshold")]
    /// Sets the jitter-buffer resynchronization threshold.
    /// Triggers timeline reset after a sufficiently large media discontinuity.
    JbResyncThreshold,
    #[serde(alias = "jbimpl")]
    /// Selects the Asterisk jitter-buffer implementation.
    /// Chooses the algorithm used for SCCP received audio.
    JbImplementation,
    #[serde(rename = "regcontext")]
    /// Sets dialplan contexts populated by station registrations.
    /// Creates or removes registration extensions as devices converge.
    RegistrationContext,
    #[serde(alias = "devicetable")]
    /// Sets the realtime backend table for SCCP devices.
    /// Selects where device definitions are loaded outside file configuration.
    DeviceTable,
    #[serde(alias = "linetable")]
    /// Sets the realtime backend table for SCCP lines.
    /// Selects where line definitions are loaded outside file configuration.
    LineTable,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum LineOption {
    Type,
    Label,
    Context,
    #[serde(rename = "callerid")]
    CallerId,
    #[serde(alias = "incominglimit")]
    IncomingLimit,
    Language,
    #[serde(rename = "accountcode")]
    AccountCode,
    #[serde(rename = "setvar")]
    SetVariable,
    Mailbox,
    #[serde(alias = "vmnum", alias = "voicemailnumber")]
    VoicemailNumber,
    #[serde(
        alias = "trnsfvm",
        alias = "voicemailtransfer",
        alias = "transfertovoicemail"
    )]
    VoicemailTransfer,
    #[serde(alias = "callgroup")]
    CallGroup,
    #[serde(alias = "pickupgroup")]
    PickupGroup,
    #[serde(alias = "namedcallgroup")]
    NamedCallGroup,
    #[serde(alias = "namedpickupgroup")]
    NamedPickupGroup,
    #[serde(alias = "directedpickup")]
    DirectedPickup,
    #[serde(alias = "directedpickupcontext")]
    DirectedPickupContext,
    #[serde(alias = "pickupmodeanswer", alias = "directedpickupmodeanswer")]
    PickupModeAnswer,
    #[serde(rename = "parkinglot")]
    ParkingLot,
    #[serde(rename = "meetme")]
    ConferenceEnabled,
    #[serde(rename = "meetmenum")]
    ConferenceNumber,
    #[serde(rename = "meetmeopts")]
    ConferenceOptions,
    #[serde(alias = "adhocnumber")]
    AdhocNumber,
    InitialDialtoneTone,
    SecondaryDialtoneDigits,
    SecondaryDialtoneTone,
    Pin,
    #[serde(rename = "regexten")]
    RegistrationExtension,
    Allow,
    Disallow,
    #[serde(alias = "videomode")]
    VideoMode,
    #[serde(alias = "audioencryption")]
    AudioEncryption,
    #[serde(rename = "echocancel")]
    EchoCancel,
    #[serde(rename = "silencesuppression")]
    SilenceSuppression,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum DeviceOption {
    /// Declares the section as an SCCP device definition.
    /// Selects device parsing and validation for the section.
    Type,
    /// Sets the human-readable description of the device.
    /// Displays it in the primary line's station header and exposes it in
    /// diagnostics and Asterisk-facing device metadata.
    Description,
    #[serde(alias = "softkeyprofile")]
    /// Selects the named soft-key profile assigned to the device.
    /// Controls which actions appear in each station call mode.
    SoftkeyProfile,
    #[serde(
        rename = "cfwdall",
        alias = "forwardallenabled",
        alias = "forward_all_enabled"
    )]
    /// Enables the call-forward-all feature on the device.
    /// Controls whether its forwarding state can be used or changed.
    ForwardAllEnabled,
    #[serde(
        rename = "cfwdbusy",
        alias = "forwardbusyenabled",
        alias = "forward_busy_enabled"
    )]
    /// Enables the call-forward-on-busy feature on the device.
    /// Controls whether its forwarding state can be used or changed.
    ForwardBusyEnabled,
    #[serde(
        rename = "cfwdnoanswer",
        alias = "forwardnoanswerenabled",
        alias = "forward_no_answer_enabled"
    )]
    /// Enables the call-forward-on-no-answer feature on the device.
    /// Controls whether its forwarding state can be used or changed.
    ForwardNoAnswerEnabled,
    #[serde(
        rename = "forward_no_answer_timeout",
        alias = "cfwdnoanswertimeout",
        alias = "forwardnoanswertimeout"
    )]
    /// Sets the no-answer delay before forwarding a call.
    /// Applies the configured number of seconds to device forwarding.
    ForwardNoAnswerTimeout,
    /// Sets the device's initial forward-all destination.
    /// Provides the destination used when forward-all is active.
    ForwardAll,
    /// Sets the device's initial forward-on-busy destination.
    /// Provides the destination used when the line cannot accept another call.
    ForwardBusy,
    /// Sets the device's initial forward-on-no-answer destination.
    /// Provides the destination used after the no-answer timeout.
    ForwardNoAnswer,
    #[serde(alias = "dndfeature")]
    /// Enables the do-not-disturb feature on the device.
    /// Controls whether the station can expose and change DND state.
    DndFeature,
    /// Sets the device's initial do-not-disturb mode.
    /// Determines how incoming calls are handled before runtime changes.
    Dnd,
    #[serde(
        rename = "privacy_feature",
        alias = "private",
        alias = "privacyfeature"
    )]
    /// Enables the privacy feature on the device.
    /// Controls whether the station can expose and change privacy state.
    PrivacyFeature,
    /// Sets the device's initial privacy state.
    /// Determines whether calls begin with device privacy enabled.
    Privacy,
    #[serde(alias = "featuredefault")]
    /// Sets the initial state of a provisioned feature button.
    /// Maps a feature-button instance to its configured enabled state.
    FeatureDefault,
    #[serde(rename = "setvar")]
    /// Adds an Asterisk channel variable for calls from the device.
    /// Applies the name and value when the SCCP channel is created.
    SetVariable,
    /// Enables call parking from the device.
    /// Controls whether the station may invoke the configured parking flow.
    Park,
    #[serde(rename = "conf_allow", alias = "confallow", alias = "conference_allow")]
    /// Allows the device to create or control conferences.
    /// Gates conference operations independently of line configuration.
    ConferenceAllow,
    #[serde(
        rename = "conf_music_on_hold_class",
        alias = "confmusiconholdclass",
        alias = "conference_music_on_hold_class"
    )]
    /// Sets the device conference music-on-hold class.
    /// Selects audio played to held conference participants.
    ConferenceMusicOnHoldClass,
    #[serde(
        rename = "conf_play_general_announce",
        alias = "confplaygeneralannounce",
        alias = "conference_play_general_announce"
    )]
    /// Controls general conference announcements for the device.
    /// Enables or suppresses room-level prompts during conferences.
    ConferencePlayGeneralAnnounce,
    #[serde(
        rename = "conf_play_part_announce",
        alias = "confplaypartannounce",
        alias = "conference_play_participant_announce"
    )]
    /// Controls participant conference announcements for the device.
    /// Enables or suppresses join and leave prompts.
    ConferencePlayParticipantAnnounce,
    #[serde(
        rename = "conf_mute_on_entry",
        alias = "confmuteonentry",
        alias = "conference_mute_on_entry"
    )]
    /// Controls whether device-created participants enter muted.
    /// Sets the initial microphone state when joining a conference.
    ConferenceMuteOnEntry,
    #[serde(
        rename = "conf_show_conflist",
        alias = "confshowconflist",
        alias = "conference_show_list"
    )]
    /// Controls whether the station shows the conference participant list.
    /// Enables or suppresses the device conference-list interface.
    ConferenceShowList,
    #[serde(rename = "meetme")]
    /// Enables conference access through the configured dial application.
    /// Allows the device to invoke conference dialing from the station UI.
    ConferenceDialingEnabled,
    #[serde(rename = "meetmeopts")]
    /// Sets application options for device conference dialing.
    /// Passes the configured flags to the Asterisk conference application.
    ConferenceOptions,
    #[serde(alias = "useredialmenu")]
    /// Controls whether redial opens the station redial menu.
    /// Selects menu-based history instead of immediately dialing the last number.
    UseRedialMenu,
    #[serde(alias = "allowringinnotification")]
    /// Enables ringing notifications for monitored hints.
    /// Allows the device to alert when a subscribed target starts ringing.
    AllowRinginNotification,
    #[serde(alias = "mwilamp")]
    /// Sets the lamp mode used for message-waiting indication.
    /// Controls how the station signals waiting voicemail.
    MwiLamp,
    #[serde(alias = "mwioncall")]
    /// Controls whether message-waiting indication remains active during calls.
    /// Keeps or suppresses MWI while the device has an active call.
    MwiOnCall,
    #[serde(alias = "phonecodepage")]
    /// Selects the legacy text code page used by the station.
    /// Controls encoding of display strings for phones without UTF-8 support.
    PhoneCodePage,
    #[serde(alias = "allowoverlap")]
    /// Controls overlap dialing for this device.
    /// Overrides whether routing may begin before all digits arrive.
    AllowOverlap,
    #[serde(alias = "forcedtmfmode", alias = "force_dtmfmode")]
    /// Forces the DTMF transport mode used by the device.
    /// Selects signaling, RTP events, or in-band digit delivery.
    ForceDtmfMode,
    #[serde(rename = "direct_media", alias = "directrtp")]
    /// Controls whether this device may use direct media.
    /// Overrides the inherited RTP anchoring policy when call conditions permit.
    DirectMedia,
    #[serde(rename = "early_media", alias = "earlyrtp")]
    /// Controls early media for this device.
    /// Overrides whether media opens before the call is answered.
    EarlyMedia,
    #[serde(alias = "audioencryption")]
    /// Sets the device audio-encryption policy.
    /// Overrides whether its SCCP audio channels negotiate encrypted media.
    AudioEncryption,
    /// Adds a source network to the device deny ACL.
    /// Rejects matching registrations for this device unless a later rule permits them.
    Deny,
    /// Adds a source network to the device permit ACL.
    /// Allows matching registrations for this device under its ACL.
    Permit,
    #[serde(alias = "permithost")]
    /// Adds a permitted signaling hostname for the device.
    /// Restricts registration to peers whose resolved host identity is allowed.
    PermitHost,
    /// Sets NAT handling for this device.
    /// Overrides inherited address selection for signaling and media endpoints.
    Nat,
    #[serde(alias = "transportrequirement", alias = "transport_requirement")]
    /// Sets the signaling transport required by the device.
    /// Restricts registration to the configured clear-text or TLS transport.
    Transport,
    #[serde(rename = "sccp_tos", alias = "signalingtos")]
    /// Sets the IP type-of-service byte for this device's signaling.
    /// Converts the legacy traffic-class value into the device DSCP policy.
    SignalingTos,
    #[serde(
        rename = "sccp_dscp",
        alias = "sccpdscp",
        alias = "signalingdscp",
        alias = "signaling_dscp"
    )]
    /// Sets the DSCP value for this device's signaling.
    /// Overrides the inherited SCCP signaling traffic classification.
    SignalingDscp,
    #[serde(rename = "sccp_cos", alias = "signalingcos", alias = "signaling_cos")]
    /// Sets the layer-two class of service for this device's signaling.
    /// Overrides the inherited SCCP signaling priority.
    SignalingCos,
    #[serde(alias = "audiotos")]
    /// Sets the IP type-of-service byte for this device's audio.
    /// Converts the legacy traffic-class value into the device audio policy.
    AudioTos,
    #[serde(alias = "audiodscp")]
    /// Sets the DSCP value for this device's audio.
    /// Overrides the inherited audio media traffic classification.
    AudioDscp,
    #[serde(alias = "audiocos")]
    /// Sets the layer-two class of service for this device's audio.
    /// Overrides the inherited audio media priority.
    AudioCos,
    #[serde(alias = "videotos")]
    /// Sets the IP type-of-service byte for this device's video.
    /// Converts the legacy traffic-class value into the device video policy.
    VideoTos,
    #[serde(alias = "videodscp")]
    /// Sets the DSCP value for this device's video.
    /// Overrides the inherited video media traffic classification.
    VideoDscp,
    #[serde(alias = "videocos")]
    /// Sets the layer-two class of service for this device's video.
    /// Overrides the inherited video media priority.
    VideoCos,
    #[serde(alias = "trustphoneip")]
    /// Recognizes the obsolete device-level phone-IP trust option.
    /// Rejects it because the signaling peer address is always authoritative.
    TrustPhoneIp,
    #[serde(alias = "dtmfmode")]
    /// Recognizes the obsolete device-level DTMF mode option.
    /// Rejects it and directs configuration to the force-DTMF option.
    ObsoleteDtmfMode,
    /// Adds codecs to the device's allowed media set.
    /// Extends the codec policy inherited from general configuration.
    Allow,
    /// Removes codecs from the device's allowed media set.
    /// Restricts the codec policy inherited from general configuration.
    Disallow,
    /// Adds a line appearance button to the device.
    /// Resolves the configured line and assigns its next device instance.
    Line,
    /// Adds an explicitly typed button to the device layout.
    /// Parses line, speed-dial, feature, service, and BLF button definitions.
    Button,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ConfigOverlayKind {
    Device,
    Line,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConfigOverlayValue {
    pub key: String,
    /// `None` deletes the matching file value. `Some("")` is an explicit
    /// empty override and therefore remains present through inheritance.
    pub value: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConfigOverlaySection {
    pub name: String,
    pub source: String,
    pub line: usize,
    pub kind: Option<ConfigOverlayKind>,
    pub parents: Vec<String>,
    pub delete: bool,
    pub values: Vec<ConfigOverlayValue>,
}

impl RawSection {
    fn diagnostic_key(&self, key: &str) -> String {
        if let Some(value) = self
            .values
            .iter()
            .rev()
            .find(|value| value.key.eq_ignore_ascii_case(key))
        {
            value.diagnostic_key()
        } else {
            format!("line {} [{}].{key}", self.line, self.name)
        }
    }

    fn section_location(&self) -> String {
        format!("line {} [{}]", self.line, self.name)
    }
}

impl RawValue {
    fn diagnostic_key(&self) -> String {
        format!("line {} [{}].{}", self.line, self.section, self.key)
    }
}

#[derive(Default)]
struct ButtonInstances {
    line: u32,
    speed_dial: u32,
    feature: u32,
    service: u32,
}

impl ButtonInstances {
    fn next(counter: &mut u32) -> u32 {
        *counter += 1;
        *counter
    }
}

struct ParsedButton {
    definition: ButtonDefinition,
    feature_argument: Option<(u32, String)>,
    blf_target: Option<(u32, HintTarget)>,
}

struct ParsedLine {
    line: LineConfig,
    features: LineFeatureConfig,
}

/// Values collected while one line section is decoded. Keeping the unresolved
/// values together makes the parse/resolve boundary explicit: Serde owns key
/// selection, this draft owns typed values and presence, and the final resolver
/// applies general inheritance and cross-field validation.
#[derive(Default)]
struct LineSectionDraft<'a> {
    incoming_limit: Option<u32>,
    mailbox: Option<Option<String>>,
    voicemail_number: Option<Option<VoicemailDestination>>,
    voicemail_transfer: Option<Option<VoicemailDestination>>,
    call_groups: Option<BTreeSet<u8>>,
    pickup_groups: Option<BTreeSet<u8>>,
    named_call_groups: Option<BTreeSet<String>>,
    named_pickup_groups: Option<BTreeSet<String>>,
    directed_pickup: Option<bool>,
    directed_pickup_context: Option<Option<String>>,
    pickup_mode_answer: Option<bool>,
    parking_lot: Option<Option<String>>,
    conference_enabled: Option<bool>,
    conference_destination: Option<Option<String>>,
    conference_options: Option<String>,
    hotline_destination: Option<Option<HotlineDestination>>,
    initial_dialtone_tone: Option<Tone>,
    secondary_dialtone_digits: Option<Option<String>>,
    secondary_dialtone_tone: Option<Tone>,
    mobility_pin: Option<Option<MobilityPin>>,
    registration_extensions: Option<Option<Vec<RegistrationExtension>>>,
    video_mode: Option<VideoMode>,
    audio_encryption: Option<MediaEncryptionPolicy>,
    echo_cancellation: Option<bool>,
    silence_suppression: Option<bool>,
    language: Option<String>,
    account_code: Option<Option<String>>,
    channel_variables: Vec<ChannelVariable>,
    codec_settings: Vec<(bool, &'a str)>,
}

#[derive(Default)]
struct QosPolicyPatch {
    signaling_dscp: Option<Dscp>,
    signaling_cos: Option<Cos>,
    audio_dscp: Option<Dscp>,
    audio_cos: Option<Cos>,
    video_dscp: Option<Dscp>,
    video_cos: Option<Cos>,
}

impl QosPolicyPatch {
    fn resolve(self, mut base: QosPolicy) -> QosPolicy {
        base.signaling.dscp = self.signaling_dscp.unwrap_or(base.signaling.dscp);
        base.signaling.cos = self.signaling_cos.unwrap_or(base.signaling.cos);
        base.audio.dscp = self.audio_dscp.unwrap_or(base.audio.dscp);
        base.audio.cos = self.audio_cos.unwrap_or(base.audio.cos);
        base.video.dscp = self.video_dscp.unwrap_or(base.video.dscp);
        base.video.cos = self.video_cos.unwrap_or(base.video.cos);
        base
    }
}

/// Unresolved values for one device section. Optional collections preserve the
/// difference between inheritance (`None`) and an explicitly cleared list
/// (`Some(Vec::new())`).
#[derive(Default)]
struct DeviceSectionDraft<'a> {
    buttons: Vec<ButtonDefinition>,
    feature_arguments: HashMap<u32, String>,
    blf_targets: HashMap<u32, HintTarget>,
    instances: ButtonInstances,
    soft_key_profile: Option<String>,
    forward_all_enabled: Option<bool>,
    forward_busy_enabled: Option<bool>,
    forward_no_answer_enabled: Option<bool>,
    forward_no_answer_timeout: Option<u32>,
    forward_all: Option<Option<ForwardingDestination>>,
    forward_busy: Option<Option<ForwardingDestination>>,
    forward_no_answer: Option<Option<ForwardingDestination>>,
    dnd_enabled: Option<bool>,
    dnd: Option<DndMode>,
    privacy_enabled: Option<bool>,
    privacy: Option<bool>,
    parking_enabled: Option<bool>,
    conference_allowed: Option<bool>,
    conference_music_on_hold_class: Option<Option<String>>,
    conference_play_general_announcements: Option<bool>,
    conference_play_participant_announcements: Option<bool>,
    conference_mute_on_entry: Option<bool>,
    conference_show_list: Option<bool>,
    conference_dialing_enabled: Option<bool>,
    conference_application_options: Option<String>,
    use_redial_menu: Option<bool>,
    allow_ringing_notification: Option<bool>,
    mwi_lamp_mode: Option<LampMode>,
    mwi_on_call: Option<bool>,
    legacy_code_page: Option<LegacyCodePage>,
    allow_overlap: Option<bool>,
    dtmf_mode: Option<DtmfMode>,
    direct_media: Option<bool>,
    early_media: Option<bool>,
    audio_encryption: Option<MediaEncryptionPolicy>,
    codec_settings: Vec<(bool, &'a str)>,
    acl_rules: Option<Vec<AclRule>>,
    permitted_hosts: Option<Vec<String>>,
    nat: Option<NatMode>,
    qos: QosPolicyPatch,
    transport: Option<TransportRequirement>,
    configured_feature_defaults: Vec<(u32, bool)>,
    channel_variables: Vec<ChannelVariable>,
}

/// Unresolved general-section values. Optional fields retain whether the user
/// actually supplied them; inherited structures are represented as patches.
#[derive(Default)]
struct GeneralSectionDraft<'a> {
    configuration_source: Option<ConfigurationSource>,
    call_answer_order: Option<CallAnswerOrder>,
    timezone_offset_minutes: Option<i16>,
    date_template: Option<DateTemplate>,
    ring_type: Option<RingerMode>,
    call_waiting_tone: Option<Option<Tone>>,
    call_waiting_interval: Option<u32>,
    first_digit_timeout: Option<u64>,
    interdigit_timeout: Option<u64>,
    dial_terminator: Option<char>,
    record_dial_terminator: Option<bool>,
    simulate_enbloc: Option<bool>,
    speed_dial_await_further_digits: Option<bool>,
    allow_overlap: Option<bool>,
    transfer_on_hangup: Option<bool>,
    fallback_decision: Option<FallbackDecision>,
    fallback_backoff: Option<u32>,
    fallback_server_priority: Option<u8>,
    conference_enabled: Option<bool>,
    conference_options: Option<String>,
    auto_answer_ring_time: Option<u32>,
    auto_answer_tone: Option<Tone>,
    remote_hangup_tone: Option<Option<Tone>>,
    hotline_enabled: Option<bool>,
    hotline_extension: Option<Option<HotlineDestination>>,
    hotline_context: Option<String>,
    hotline_label: Option<String>,
    direct_media: Option<bool>,
    early_media: Option<bool>,
    audio_encryption: Option<MediaEncryptionPolicy>,
    echo_cancellation: Option<bool>,
    silence_suppression: Option<bool>,
    jitter_enabled: Option<bool>,
    jitter_forced: Option<bool>,
    jitter_log_frames: Option<bool>,
    jitter_max_size_ms: Option<u32>,
    jitter_resync_threshold_ms: Option<u32>,
    jitter_implementation: Option<JitterBufferImplementation>,
    registration_contexts: Option<Vec<String>>,
    codec_settings: Vec<(bool, &'a str)>,
    clear_bind: Option<SocketAddr>,
    clear_address: Option<IpAddr>,
    clear_port: Option<u16>,
    tls_bind: Option<SocketAddr>,
    tls_address: Option<IpAddr>,
    tls_port: Option<u16>,
    combined_pem: Option<PathBuf>,
    tls_certificate: Option<PathBuf>,
    tls_private_key: Option<PathBuf>,
    tls_trust_store: Option<PathBuf>,
    acl_rules: Option<Vec<AclRule>>,
    local_networks: Option<Vec<IpNetwork>>,
    external_address: Option<Option<IpAddr>>,
    external_hostname: Option<Option<String>>,
    external_refresh: Option<u32>,
    nat: Option<NatMode>,
    advertised_ipv4: Option<Option<Ipv4Addr>>,
    advertised_ipv6: Option<Option<Ipv6Addr>>,
    advertised_alias_seen: bool,
    qos: QosPolicyPatch,
    device_table: Option<String>,
    line_table: Option<String>,
    language: Option<String>,
    account_code: Option<Option<String>>,
}

impl ModuleConfig {
    pub fn parse(input: &str) -> Result<Self, ConfigError> {
        Self::from_raw_sections(parse_sections(input)?)
    }

    /// Validate that every option in a source file uses the Serde schema's
    /// canonical spelling. Runtime parsing remains case-insensitive and may
    /// accept explicitly declared compatibility aliases.
    pub fn check_canonical(input: &str) -> Result<(), ConfigError> {
        Self::parse(input)?;
        let sections = parse_sections(input)?;
        for section in &sections {
            let kind = source_section_kind(section, &sections)?;
            check_canonical_section(section, &kind)?;
        }
        Ok(())
    }

    /// Render a validated, deterministic configuration using canonical option
    /// names. Templates are resolved and the source is never modified.
    pub fn to_canonical_string(input: &str) -> Result<String, ConfigError> {
        Self::parse(input)?;
        let mut sections = resolve_inheritance(parse_sections(input)?)?;
        sections.sort_by(|left, right| {
            canonical_section_rank(left)
                .cmp(&canonical_section_rank(right))
                .then_with(|| {
                    left.name
                        .to_ascii_lowercase()
                        .cmp(&right.name.to_ascii_lowercase())
                })
        });

        let mut output = String::new();
        for (index, section) in sections.iter().enumerate() {
            if index != 0 {
                output.push('\n');
            }
            output.push('[');
            output.push_str(&section.name);
            output.push_str("]\n");
            for entry in canonical_section_entries(section)? {
                output.push_str(&entry.key);
                output.push_str(" = ");
                output.push_str(&canonical::value(entry.value));
                output.push('\n');
            }
        }
        Ok(output)
    }

    pub(crate) fn parse_with_overlays(
        input: &str,
        overlays: &[ConfigOverlaySection],
    ) -> Result<Self, ConfigError> {
        let mut sections = parse_sections(input)?;
        apply_config_overlays(&mut sections, overlays)?;
        Self::from_raw_sections(sections)
    }

    pub(crate) fn realtime_tables_from_source(
        input: &str,
    ) -> Result<Option<RealtimeTableConfig>, ConfigError> {
        let sections = parse_sections(input)?;
        let mut general = GeneralConfig::default();
        if let Some(section) = sections
            .iter()
            .find(|section| section.name.eq_ignore_ascii_case("general"))
        {
            parsing::general::parse_general(&mut general, section)
                .map_err(|error| locate_section_error(error, section))?;
        }
        Ok(general.realtime_tables)
    }

    pub(crate) fn configuration_source_from_source(
        input: &str,
    ) -> Result<ConfigurationSource, ConfigError> {
        let sections = parse_sections(input)?;
        let mut general = GeneralConfig::default();
        if let Some(section) = sections
            .iter()
            .find(|section| section.name.eq_ignore_ascii_case("general"))
        {
            parsing::general::parse_general(&mut general, section)
                .map_err(|error| locate_section_error(error, section))?;
        }
        Ok(general.configuration_source)
    }

    pub(crate) fn parse_with_sorcery_overlays(
        input: &str,
        overlays: &[ConfigOverlaySection],
    ) -> Result<Self, ConfigError> {
        let mut sections = parse_sections(input)?;
        let source_sections = sections.clone();
        let mut retained = Vec::with_capacity(sections.len());
        for section in sections.drain(..) {
            let managed = !section.is_template
                && !section.name.eq_ignore_ascii_case("general")
                && matches!(
                    source_section_kind(&section, &source_sections)?.as_str(),
                    "device" | "line"
                );
            if !managed {
                retained.push(section);
            }
        }
        if let Some(overlay) = overlays.iter().find(|overlay| {
            retained
                .iter()
                .any(|section| section.name.eq_ignore_ascii_case(&overlay.name))
        }) {
            return Err(ConfigError::DuplicateSection(overlay.name.clone()));
        }
        apply_config_overlays(&mut retained, overlays)?;
        Self::from_raw_sections(retained)
    }

    fn from_raw_sections(sections: Vec<RawSection>) -> Result<Self, ConfigError> {
        let sections = resolve_inheritance(sections)?;
        let mut general = GeneralConfig::default();
        let mut devices = HashMap::new();
        let mut lines = HashMap::new();
        let mut line_features = HashMap::new();
        let mut registration_target_owners = HashMap::<RegistrationTarget, String>::new();
        let mut device_codec_overrides = HashSet::new();
        let mut line_codec_overrides = HashSet::new();
        let mut device_audio_encryption_overrides = HashSet::new();
        let mut line_audio_encryption_overrides = HashSet::new();
        let mut soft_key_profiles = HashMap::from([(
            DEFAULT_SOFT_KEY_PROFILE.to_owned(),
            SoftKeyProfile::built_in(),
        )]);

        // Resolve general defaults before typing lines and devices so section
        // order cannot affect inherited media policy.
        for section in &sections {
            if section.name.eq_ignore_ascii_case("general") {
                parsing::general::parse_general(&mut general, section)
                    .map_err(|error| locate_section_error(error, section))?;
            }
        }

        // Lines are collected before devices so button declarations may refer
        // to line sections that appear later in the file.
        for section in &sections {
            if section.name.eq_ignore_ascii_case("general") {
                continue;
            }

            let kind = value(section, "type")
                .ok_or_else(|| ConfigError::MissingSectionType(section.name.clone()))?;
            match kind.to_ascii_lowercase().as_str() {
                "device" => {}
                "line" => {
                    let config = parsing::line::parse_line(section, &general)
                        .map_err(|error| locate_section_error(error, section))?;
                    let number = config.line.number.clone();
                    if lines.contains_key(&number) {
                        return Err(ConfigError::DuplicateSection(section.name.clone()));
                    }
                    for target in resolve_registration_targets(
                        &general.registration.contexts,
                        &config.features.registration.extensions,
                    ) {
                        if let Some(previous) =
                            registration_target_owners.insert(target.clone(), number.clone())
                        {
                            return Err(invalid_option(
                                section.diagnostic_key("regexten"),
                                &format!("{}@{}", target.extension, target.context),
                                &format!(
                                    "a registration target unique across lines; already used by [{previous}]"
                                ),
                                false,
                            ));
                        }
                    }
                    if section_has_codec_settings(section) {
                        line_codec_overrides.insert(number.clone());
                    }
                    if section_has_audio_encryption_setting(section) {
                        line_audio_encryption_overrides.insert(number.clone());
                    }
                    lines.insert(number.clone(), config.line);
                    line_features.insert(number, config.features);
                }
                "softkey_profile" => {
                    let config = parse_soft_key_profile(section)
                        .map_err(|error| locate_section_error(error, section))?;
                    soft_key_profiles.insert(canonical::profile_name(&config.name), config);
                }
                other => {
                    return Err(ConfigError::UnknownSectionType {
                        section: section.name.clone(),
                        kind: other.to_owned(),
                    });
                }
            }
        }

        for section in &sections {
            if section.name.eq_ignore_ascii_case("general")
                || !value(section, "type").is_some_and(|kind| kind.eq_ignore_ascii_case("device"))
            {
                continue;
            }
            let config =
                parsing::device::parse_device(section, &lines, &soft_key_profiles, &general)
                    .map_err(|error| locate_section_error(error, section))?;
            if section_has_codec_settings(section) {
                device_codec_overrides.insert(config.id.clone());
            }
            if section_has_audio_encryption_setting(section) {
                device_audio_encryption_overrides.insert(config.id.clone());
            }
            if devices.insert(config.id.clone(), config).is_some() {
                return Err(ConfigError::DuplicateSection(section.name.clone()));
            }
        }

        if general.configuration_source == ConfigurationSource::File
            && (devices.is_empty() || lines.is_empty())
        {
            return Err(ConfigError::Empty);
        }
        if general.bind.port() == 0
            || (general.network.advertised.ipv4.is_none()
                && general.network.advertised.ipv6.is_none())
        {
            return Err(ConfigError::InvalidValue {
                key: "[general] listener/advertised address policy".into(),
                value: format!(
                    "clear={} advertised_ipv4={:?} advertised_ipv6={:?}; expected a nonzero listener port and at least one advertised address",
                    general.bind, general.network.advertised.ipv4, general.network.advertised.ipv6
                ),
            });
        }
        let timing = general.timing_policy();
        if timing.keepalive < Duration::from_secs(5)
            || timing.secondary_keepalive < Duration::from_secs(5)
            || timing.interdigit_timeout < Duration::from_millis(250)
        {
            return Err(ConfigError::InvalidValue {
                key: "keepalive/secondary_keepalive/interdigit_timeout".into(),
                value: format!(
                    "{}/{}/{}",
                    general.keepalive_seconds,
                    general.secondary_keepalive_seconds,
                    general.interdigit_timeout_ms
                ),
            });
        }
        if general.signaling_servers.len() > sccp_protocol::MAX_SIGNALING_SERVERS {
            return Err(ConfigError::InvalidValue {
                key: "signaling_server".into(),
                value: "too many configured endpoints".into(),
            });
        }
        let priorities = general
            .signaling_servers
            .iter()
            .map(|server| server.priority)
            .collect::<HashSet<_>>();
        if priorities.len() != general.signaling_servers.len()
            || !general.signaling_servers.is_empty()
                && !priorities.contains(&general.fallback_registration.server_priority)
        {
            return Err(ConfigError::InvalidValue {
                key: "signaling_server/server_priority".into(),
                value: "priorities must be unique and include this server".into(),
            });
        }

        let mut bindings = Vec::new();
        let mut bindings_by_line = HashMap::<String, Vec<usize>>::new();
        let mut bindings_by_device = HashMap::<DeviceId, Vec<usize>>::new();
        let mut binding_by_button = HashMap::new();
        let mut device_ids: Vec<_> = devices.keys().cloned().collect();
        device_ids.sort();
        for device_id in device_ids {
            let device = devices.get(&device_id).expect("device ID came from map");
            let mut seen = HashSet::new();
            for line_definition in device.buttons.iter().filter_map(|button| match button {
                ButtonDefinition::Line(line) => Some(line),
                _ => None,
            }) {
                let line_name = &line_definition.number;
                if !seen.insert(line_name) {
                    return Err(ConfigError::InvalidValue {
                        key: format!("{}.line", device.id),
                        value: line_name.clone(),
                    });
                }
                let line =
                    lines
                        .get(line_name)
                        .cloned()
                        .ok_or_else(|| ConfigError::UnknownLine {
                            device: device.id.clone(),
                            line: line_name.clone(),
                        })?;
                let binding = LineBinding {
                    device_id: device.id.clone(),
                    line_instance: line_definition.instance,
                    appearance: line_definition.clone(),
                    line,
                };
                let index = bindings.len();
                bindings.push(binding);
                bindings_by_line
                    .entry(line_name.clone())
                    .or_default()
                    .push(index);
                bindings_by_device
                    .entry(device.id.clone())
                    .or_default()
                    .push(index);
                if binding_by_button
                    .insert((device.id.clone(), line_definition.instance), index)
                    .is_some()
                {
                    return Err(ConfigError::InvalidValue {
                        key: format!("{}.line_instance", device.id),
                        value: line_definition.instance.to_string(),
                    });
                }
            }
        }
        if general.configuration_source == ConfigurationSource::File
            && let Some(unassigned) = lines
                .keys()
                .find(|line| !bindings_by_line.contains_key(*line))
        {
            return Err(ConfigError::UnassignedLine(unassigned.clone()));
        }

        Ok(Self {
            general,
            devices,
            lines,
            line_features,
            soft_key_profiles,
            bindings,
            bindings_by_line,
            bindings_by_device,
            binding_by_button,
            device_codec_overrides,
            line_codec_overrides,
            device_audio_encryption_overrides,
            line_audio_encryption_overrides,
        })
    }

    pub fn line(&self, number: &str) -> Option<&LineBinding> {
        self.appearances_for_line(number).next()
    }

    pub fn soft_key_profile(&self, name: &str) -> Option<&SoftKeyProfile> {
        self.soft_key_profiles.get(&canonical::profile_name(name))
    }

    pub fn soft_key_profile_for_device(&self, device: &DeviceId) -> Option<&SoftKeyProfile> {
        let profile = &self.devices.get(device)?.soft_key_profile;
        self.soft_key_profiles.get(profile)
    }

    pub fn feature_defaults_for_device(&self, device: &DeviceId) -> Option<&DeviceFeatureDefaults> {
        Some(&self.devices.get(device)?.feature_defaults)
    }

    pub fn dnd_button_mode(
        &self,
        device: &DeviceId,
        feature_instance: u32,
    ) -> Option<DndButtonMode> {
        self.dnd_buttons_for_device(device)
            .find_map(|(instance, mode)| (instance == feature_instance).then_some(mode))
    }

    pub fn dnd_buttons_for_device<'a>(
        &'a self,
        device: &DeviceId,
    ) -> impl Iterator<Item = (u32, DndButtonMode)> + 'a {
        self.devices.get(device).into_iter().flat_map(|device| {
            device.buttons.iter().filter_map(|button| match button {
                ButtonDefinition::Feature(feature)
                    if feature.feature == ButtonType::DoNotDisturb =>
                {
                    Some((
                        feature.instance,
                        match device.feature_arguments.get(&feature.instance) {
                            Some(argument) if argument == "silent" => DndButtonMode::Silent,
                            Some(argument) if argument == "reject" => DndButtonMode::Reject,
                            Some(_) => {
                                unreachable!("DND feature arguments are normalized during parsing")
                            }
                            None => DndButtonMode::Cycle,
                        },
                    ))
                }
                _ => None,
            })
        })
    }

    /// Returns every physical recording control in station layout order.
    pub fn recording_buttons_for_device<'a>(
        &'a self,
        device: &DeviceId,
    ) -> impl Iterator<Item = &'a RecordingButtonDefinition> + 'a {
        self.devices.get(device).into_iter().flat_map(|device| {
            device.buttons.iter().filter_map(|button| match button {
                ButtonDefinition::Recording(recording) => Some(recording),
                _ => None,
            })
        })
    }

    pub fn features_for_line(&self, number: &str) -> Option<&LineFeatureConfig> {
        self.line_features.get(number)
    }

    pub fn parking_for_device(&self, device: &DeviceId) -> Option<&DeviceParkingConfig> {
        Some(&self.devices.get(device)?.parking)
    }

    pub fn parking_for_line(&self, number: &str) -> Option<&LineParkingConfig> {
        Some(&self.line_features.get(number)?.parking)
    }

    pub fn parking_lot_for_button(
        &self,
        device: &DeviceId,
        feature_instance: u32,
    ) -> Option<&ParkingLotButtonConfig> {
        self.devices
            .get(device)?
            .parking
            .feature_buttons
            .get(&feature_instance)
    }

    pub fn conference_for_device(&self, device: &DeviceId) -> Option<&DeviceConferenceConfig> {
        Some(&self.devices.get(device)?.conference)
    }

    pub fn conference_for_line(&self, number: &str) -> Option<&LineConferenceConfig> {
        Some(&self.line_features.get(number)?.conference)
    }

    pub fn call_answer_order(&self) -> CallAnswerOrder {
        self.general.call_answer_order
    }

    pub fn call_ui_for_device(&self, device: &DeviceId) -> Option<&DeviceCallUiConfig> {
        Some(&self.devices.get(device)?.call_ui)
    }

    pub fn auto_answer(&self) -> &AutoAnswerConfig {
        &self.general.auto_answer
    }

    pub fn guest_hotline(&self) -> &GuestHotlineConfig {
        &self.general.guest_hotline
    }

    /// Build the policy-neutral logical line used by an otherwise unknown
    /// station admitted through the anonymous guest-hotline policy. The PBX
    /// destination is deliberately not copied into the binding.
    pub fn guest_hotline_binding(
        &self,
        device_id: &DeviceId,
        line_instance: u32,
    ) -> Option<LineBinding> {
        let guest = self.guest_hotline();
        if self.devices.contains_key(device_id)
            || !guest.enabled
            || guest.extension.is_none()
            || line_instance != 1
        {
            return None;
        }
        let line = LineConfig {
            number: "hotline".into(),
            label: guest.label.clone(),
            context: guest.context.clone(),
            caller_name: guest.label.clone(),
            caller_number: "hotline".into(),
            mailbox: None,
            language: self.general.language.clone(),
            account_code: self.general.account_code.clone(),
            channel_variables: Vec::new(),
        };
        let mut appearance = LineAppearance::new(
            line_instance,
            LineDefinition {
                number: line.number.clone(),
                display_name: guest.label.clone(),
            },
        );
        appearance.label = Some(guest.label.clone());
        Some(LineBinding {
            device_id: device_id.clone(),
            line_instance,
            appearance,
            line,
        })
    }

    pub fn hotline_for_line(&self, number: &str) -> Option<&LineHotlineConfig> {
        Some(&self.line_features.get(number)?.hotline)
    }

    pub fn hotline_destination_for_binding(
        &self,
        binding: &LineBinding,
    ) -> Option<&HotlineDestination> {
        if self.devices.contains_key(&binding.device_id) {
            return self
                .hotline_for_line(&binding.line.number)?
                .destination
                .as_ref();
        }
        let guest = self.guest_hotline();
        (guest.enabled && binding.line_instance == 1 && binding.line.number == "hotline")
            .then_some(guest.extension.as_ref())
            .flatten()
    }

    pub fn registration_contexts(&self) -> &[String] {
        &self.general.registration.contexts
    }

    pub fn fallback_registration(&self) -> &FallbackRegistrationConfig {
        &self.general.fallback_registration
    }

    pub fn mobility_for_line(&self, number: &str) -> Option<&LineMobilityConfig> {
        Some(&self.line_features.get(number)?.mobility)
    }

    pub fn registration_for_line(&self, number: &str) -> Option<&LineRegistrationConfig> {
        Some(&self.line_features.get(number)?.registration)
    }

    pub fn registration_targets_for_line(&self, number: &str) -> Option<Vec<RegistrationTarget>> {
        let registration = &self.line_features.get(number)?.registration;
        Some(resolve_registration_targets(
            &self.general.registration.contexts,
            &registration.extensions,
        ))
    }

    pub fn media_for_device(&self, device: &DeviceId) -> Option<&DeviceMediaConfig> {
        Some(&self.devices.get(device)?.media)
    }

    pub fn network_policy(&self) -> &NetworkPolicy {
        &self.general.network
    }

    pub fn listener_policy(&self) -> &ListenerPolicy {
        &self.general.listeners
    }

    pub fn qos_policy(&self) -> &QosPolicy {
        &self.general.qos
    }

    pub fn realtime_tables(&self) -> Option<&RealtimeTableConfig> {
        self.general.realtime_tables.as_ref()
    }

    pub fn network_for_device(&self, device: &DeviceId) -> Option<&DeviceNetworkPolicy> {
        Some(&self.devices.get(device)?.network)
    }

    pub fn media_for_line(&self, number: &str) -> Option<&LineMediaConfig> {
        Some(&self.line_features.get(number)?.media)
    }

    pub fn media_for_appearance(
        &self,
        device: &DeviceId,
        line_instance: u32,
    ) -> Option<ResolvedMediaConfig> {
        let binding = self.line_for_device(device, line_instance)?;
        self.media_for_binding(binding)
    }

    /// Resolve media policy for either a configured or runtime-created line
    /// appearance. Runtime mobility bindings still name a configured device
    /// and logical line, so they use the same normalization precedence.
    pub fn media_for_binding(&self, binding: &LineBinding) -> Option<ResolvedMediaConfig> {
        let device = &binding.device_id;
        let device_config = self.devices.get(device)?;
        let line = self.line_features.get(&binding.line.number)?;
        let codecs = if self.line_codec_overrides.contains(&binding.line.number) {
            line.media.codecs.clone()
        } else if self.device_codec_overrides.contains(device) {
            device_config.media.codecs.clone()
        } else {
            self.general.codecs.clone()
        };
        let audio_encryption = if self
            .line_audio_encryption_overrides
            .contains(&binding.line.number)
        {
            line.media.audio_encryption.clone()
        } else if self.device_audio_encryption_overrides.contains(device) {
            device_config.media.audio_encryption.clone()
        } else {
            self.general.audio_encryption.clone()
        };
        Some(ResolvedMediaConfig {
            codecs,
            audio_encryption,
            dtmf_mode: device_config.media.dtmf_mode,
            direct_media: device_config.media.direct_media,
            early_media: device_config.media.early_media,
            video_mode: line.media.video_mode,
            audio_processing: line.media.audio_processing,
        })
    }

    /// Resolve the general, device, and line conference-dialing layers for a
    /// concrete line appearance.
    pub fn conference_dialing_for_appearance(
        &self,
        device: &DeviceId,
        line_instance: u32,
    ) -> Option<ResolvedConferenceDialing> {
        let binding = self.line_for_device(device, line_instance)?;
        self.conference_dialing_for_binding(binding)
    }

    pub fn conference_dialing_for_binding(
        &self,
        binding: &LineBinding,
    ) -> Option<ResolvedConferenceDialing> {
        let device = self.devices.get(&binding.device_id)?;
        let line = self.line_features.get(&binding.line.number)?;
        Some(ResolvedConferenceDialing {
            enabled: line
                .conference
                .enabled
                .unwrap_or(device.conference.dialing.enabled),
            destination: line.conference.destination.clone(),
            application_options: line
                .conference
                .application_options
                .clone()
                .unwrap_or_else(|| device.conference.dialing.application_options.clone()),
        })
    }

    pub fn line_appearance_count(&self, number: &str) -> usize {
        self.bindings_by_line.get(number).map_or(0, Vec::len)
    }

    pub fn appearances_for_line(&self, number: &str) -> impl Iterator<Item = &LineBinding> {
        self.bindings_by_line
            .get(number)
            .into_iter()
            .flatten()
            .filter_map(|index| self.bindings.get(*index))
    }

    pub fn appearances_for_device(&self, device: &DeviceId) -> impl Iterator<Item = &LineBinding> {
        self.bindings_by_device
            .get(device)
            .into_iter()
            .flatten()
            .filter_map(|index| self.bindings.get(*index))
    }

    pub fn line_for_device(&self, device: &DeviceId, instance: u32) -> Option<&LineBinding> {
        self.binding_by_button
            .get(&(device.clone(), instance))
            .and_then(|index| self.bindings.get(*index))
    }

    /// Resolve either `line` or the legacy-compatible `device/line` dial form.
    pub fn dial_target(&self, address: &str) -> Option<&LineBinding> {
        let mut parts = address.split('/').map(str::trim);
        let first = parts.next()?;
        let second = parts.next();
        if parts.next().is_some() {
            return None;
        }
        let Some(line) = second else {
            return self.line(first);
        };
        let device = DeviceId::new(first).ok()?;
        self.appearances_for_line(line)
            .find(|binding| binding.device_id == device)
    }

    pub fn device_definitions(&self) -> Vec<DeviceDefinition> {
        let mut definitions: Vec<_> = self
            .devices
            .values()
            .map(|device| DeviceDefinition {
                id: device.id.clone(),
                description: device.description.clone(),
                transport: device.network.transport.into(),
                signaling_qos: Some(SignalingQos::new(
                    device.network.qos.signaling.dscp.0,
                    device.network.qos.signaling.cos.0,
                )),
                buttons: device
                    .buttons
                    .iter()
                    .cloned()
                    .map(|button| match button {
                        ButtonDefinition::Line(mut appearance) => {
                            if let Some(features) = self.line_features.get(&appearance.number) {
                                appearance.initial_tone = features.dial_tones.initial;
                            }
                            ButtonDefinition::Line(appearance)
                        }
                        button => button,
                    })
                    .collect(),
                soft_keys: self
                    .soft_key_profiles
                    .get(&device.soft_key_profile)
                    .expect("device soft-key profile was validated during parsing")
                    .station_profile(),
                ui: StationUiPolicy {
                    placed_calls_redial_menu: matches!(
                        device.call_ui.redial_mode,
                        RedialMode::PlacedCallsMenu
                    ),
                    hinted_ringing_notification: device.call_ui.hinted_ringing_notification,
                    speed_dial_await_further_digits: self.general.speed_dial_await_further_digits,
                    mwi_lamp_mode: device.call_ui.mwi_lamp_mode,
                    mwi_on_call: device.call_ui.mwi_on_call,
                    legacy_code_page: device.call_ui.legacy_code_page,
                },
            })
            .collect();
        definitions.sort_by(|left, right| left.id.cmp(&right.id));
        definitions
    }
}

fn section_has_codec_settings(section: &RawSection) -> bool {
    section
        .values
        .iter()
        .any(|value| matches!(normalize_name(&value.key).as_str(), "allow" | "disallow"))
}

fn section_has_audio_encryption_setting(section: &RawSection) -> bool {
    section
        .values
        .iter()
        .any(|value| normalize_name(&value.key) == "audioencryption")
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "snake_case")]
struct SoftKeyProfileSection {
    #[serde(rename = "type")]
    section_type: Option<String>,
    on_hook: Option<String>,
    connected: Option<String>,
    on_hold: Option<String>,
    ring_in: Option<String>,
    off_hook: Option<String>,
    connected_transfer: Option<String>,
    digits_following: Option<String>,
    connected_conference: Option<String>,
    ring_out: Option<String>,
    off_hook_feature: Option<String>,
    in_use_hint: Option<String>,
    on_hook_stealable: Option<String>,
    hold_conference: Option<String>,
    empty: Option<String>,
}

fn parse_soft_key_profile(section: &RawSection) -> Result<SoftKeyProfile, ConfigError> {
    let name = canonical::profile_name(&section.name);
    if name.is_empty() {
        return Err(ConfigError::InvalidValue {
            key: "softkey_profile.name".into(),
            value: section.name.clone(),
        });
    }
    let decoded: SoftKeyProfileSection = deserialize_section(section)?;
    if decoded
        .section_type
        .as_deref()
        .is_none_or(|kind| !kind.eq_ignore_ascii_case("softkey_profile"))
    {
        return Err(ConfigError::InvalidValue {
            key: section.diagnostic_key("type"),
            value: format!(
                "{:?}; expected one type = softkey_profile",
                decoded.section_type.as_deref().unwrap_or("")
            ),
        });
    }

    let mut profile = SoftKeyProfile::empty(name);
    for (mode, raw) in [
        (KeyMode::OnHook, decoded.on_hook),
        (KeyMode::Connected, decoded.connected),
        (KeyMode::OnHold, decoded.on_hold),
        (KeyMode::RingIn, decoded.ring_in),
        (KeyMode::OffHook, decoded.off_hook),
        (KeyMode::ConnectedTransfer, decoded.connected_transfer),
        (KeyMode::DigitsFollowing, decoded.digits_following),
        (KeyMode::ConnectedConference, decoded.connected_conference),
        (KeyMode::RingOut, decoded.ring_out),
        (KeyMode::OffHookFeature, decoded.off_hook_feature),
        (KeyMode::InUseHint, decoded.in_use_hint),
        (KeyMode::OnHookStealable, decoded.on_hook_stealable),
        (KeyMode::HoldConference, decoded.hold_conference),
        (KeyMode::Empty, decoded.empty),
    ] {
        let Some(raw) = raw else {
            continue;
        };
        let diagnostic = section.diagnostic_key(key_mode_option(mode));
        let mut actions = Vec::new();
        let mut seen_actions = HashSet::new();
        if !raw.trim().is_empty() {
            for name in raw.split(',') {
                let name = name.trim();
                let action = parse_soft_key(name).ok_or_else(|| ConfigError::InvalidValue {
                    key: diagnostic.clone(),
                    value: format!("{name:?}; expected a recognized soft-key action"),
                })?;
                if !seen_actions.insert(action) {
                    return Err(ConfigError::InvalidValue {
                        key: diagnostic,
                        value: format!("{name:?}; expected unique soft-key actions"),
                    });
                }
                actions.push(action);
                if actions.len() > MAX_SOFT_KEYS_PER_MODE {
                    return Err(ConfigError::InvalidValue {
                        key: diagnostic,
                        value: format!(
                            "{raw:?}; expected at most {MAX_SOFT_KEYS_PER_MODE} actions"
                        ),
                    });
                }
            }
        }
        profile.sets.insert(mode, actions);
    }

    Ok(profile)
}

fn key_mode_option(mode: KeyMode) -> &'static str {
    match mode {
        KeyMode::OnHook => "on_hook",
        KeyMode::Connected => "connected",
        KeyMode::OnHold => "on_hold",
        KeyMode::RingIn => "ring_in",
        KeyMode::OffHook => "off_hook",
        KeyMode::ConnectedTransfer => "connected_transfer",
        KeyMode::DigitsFollowing => "digits_following",
        KeyMode::ConnectedConference => "connected_conference",
        KeyMode::RingOut => "ring_out",
        KeyMode::OffHookFeature => "off_hook_feature",
        KeyMode::InUseHint => "in_use_hint",
        KeyMode::OnHookStealable => "on_hook_stealable",
        KeyMode::HoldConference => "hold_conference",
        KeyMode::Empty => "empty",
        KeyMode::Unknown(_) => "unknown",
    }
}

fn parse_soft_key(raw: &str) -> Option<SoftKey> {
    Some(match normalize_name(raw).as_str() {
        "redial" => SoftKey::Redial,
        "newcall" => SoftKey::NewCall,
        "hold" => SoftKey::Hold,
        "transfer" => SoftKey::Transfer,
        "forwardall" | "cfwdall" => SoftKey::ForwardAll,
        "forwardbusy" | "cfwdbusy" => SoftKey::ForwardBusy,
        "forwardnoanswer" | "cfwdnoanswer" => SoftKey::ForwardNoAnswer,
        "backspace" => SoftKey::Backspace,
        "endcall" => SoftKey::EndCall,
        "resume" => SoftKey::Resume,
        "answer" => SoftKey::Answer,
        "info" => SoftKey::Info,
        "conference" => SoftKey::Conference,
        "park" => SoftKey::Park,
        "join" => SoftKey::Join,
        "meetme" => SoftKey::MeetMe,
        "pickup" => SoftKey::Pickup,
        "grouppickup" => SoftKey::GroupPickup,
        "monitor" => SoftKey::Monitor,
        "callback" => SoftKey::Callback,
        "barge" => SoftKey::Barge,
        "donotdisturb" | "dnd" => SoftKey::DoNotDisturb,
        "conferencelist" => SoftKey::ConferenceList,
        "select" => SoftKey::Select,
        "private" => SoftKey::Private,
        "transfertovoicemail" => SoftKey::TransferToVoicemail,
        "directtransfer" => SoftKey::DirectTransfer,
        "immediatedivert" => SoftKey::ImmediateDivert,
        "videomode" => SoftKey::VideoMode,
        "intercept" => SoftKey::Intercept,
        "empty" => SoftKey::Empty,
        "dial" => SoftKey::Dial,
        _ => return None,
    })
}

fn parse_feature(raw: &str) -> Result<ButtonType, ConfigError> {
    let feature = match normalize_name(raw).as_str() {
        "redial" | "lastnumberredial" => ButtonType::LastNumberRedial,
        "hold" => ButtonType::Hold,
        "transfer" => ButtonType::Transfer,
        "forwardall" | "cfwdall" => ButtonType::ForwardAll,
        "forwardbusy" | "cfwdbusy" => ButtonType::ForwardBusy,
        "forwardnoanswer" | "cfwdnoanswer" => ButtonType::ForwardNoAnswer,
        "video" => ButtonType::Video,
        "voicemail" => ButtonType::Voicemail,
        "answerrelease" => ButtonType::AnswerRelease,
        "autoanswer" => ButtonType::AutoAnswer,
        "select" => ButtonType::Select,
        "feature" => ButtonType::Feature,
        "maliciouscall" => ButtonType::MaliciousCall,
        "meetme" | "meetmeconference" => ButtonType::MeetMeConference,
        "conference" => ButtonType::Conference,
        "park" | "callpark" => ButtonType::CallPark,
        "pickup" | "callpickup" => ButtonType::CallPickup,
        "grouppickup" | "groupcallpickup" => ButtonType::GroupCallPickup,
        "mobility" => ButtonType::Mobility,
        "dnd" | "donotdisturb" => ButtonType::DoNotDisturb,
        "conferencelist" => ButtonType::ConferenceList,
        "removelastparticipant" => ButtonType::RemoveLastParticipant,
        "qualityreport" | "qualityreporttool" => ButtonType::QualityReportTool,
        "callback" => ButtonType::Callback,
        "otherpickup" => ButtonType::OtherPickup,
        "videomode" => ButtonType::VideoMode,
        "newcall" => ButtonType::NewCall,
        "endcall" => ButtonType::EndCall,
        "huntgrouplogin" => ButtonType::HuntGroupLogin,
        "queue" | "queuing" => ButtonType::Queuing,
        "parkinglot" => ButtonType::ParkingLot,
        "messages" => ButtonType::Messages,
        "directory" => ButtonType::Directory,
        "application" => ButtonType::Application,
        "headset" => ButtonType::Headset,
        "echocancellation" | "acousticechocancellation" => ButtonType::AcousticEchoCancellation,
        _ => {
            return Err(ConfigError::InvalidValue {
                key: "button.feature".into(),
                value: raw.into(),
            });
        }
    };
    Ok(feature)
}

fn parse_addon_type(raw: &str) -> Result<DeviceType, ConfigError> {
    let device_type = match normalize_name(raw).as_str() {
        "7914" | "cisco7914" | "ciscoaddon7914" => DeviceType::CiscoAddon7914,
        "791512" | "cisco791512" | "ciscoaddon791512" => DeviceType::CiscoAddon7915_12,
        "791524" | "cisco791524" | "ciscoaddon791524" => DeviceType::CiscoAddon7915_24,
        "791612" | "cisco791612" | "ciscoaddon791612" => DeviceType::CiscoAddon7916_12,
        "791624" | "cisco791624" | "ciscoaddon791624" => DeviceType::CiscoAddon7916_24,
        "spa500s" | "addonspa500s" => DeviceType::AddonSpa500s,
        "spa500ds" | "addonspa500ds" => DeviceType::AddonSpa500ds,
        "spa932ds" | "addonspa932ds" => DeviceType::AddonSpa932ds,
        _ => {
            return Err(ConfigError::InvalidValue {
                key: "button.addon.type".into(),
                value: raw.into(),
            });
        }
    };
    Ok(device_type)
}

fn required_button_field<'a>(field: &'a str, raw: &str) -> Result<&'a str, ConfigError> {
    let field = field.trim();
    if field.is_empty() {
        Err(invalid_button(raw))
    } else {
        Ok(field)
    }
}

fn parse_blf_hint(field: &str, raw: &str) -> Result<HintTarget, ConfigError> {
    let hint = required_button_field(field, raw)?;
    HintTarget::parse(hint)
}

fn invalid_button(raw: &str) -> ConfigError {
    ConfigError::InvalidValue {
        key: "button".into(),
        value: raw.into(),
    }
}

fn normalize_name(raw: &str) -> String {
    raw.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn parse_sections(input: &str) -> Result<Vec<RawSection>, ConfigError> {
    let mut sections = Vec::<RawSection>::new();
    let mut current: Option<RawSection> = None;
    let mut names = HashSet::new();
    for (index, raw_line) in input.lines().enumerate() {
        let line_number = index + 1;
        let line = strip_inline_comment(raw_line).trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            let Some(close) = line.find(']') else {
                return Err(ConfigError::Syntax {
                    line: line_number,
                    message: "malformed section header".into(),
                });
            };
            let name = line[1..close].trim().to_owned();
            if name.is_empty() {
                return Err(ConfigError::Syntax {
                    line: line_number,
                    message: "section name cannot be empty".into(),
                });
            }
            let suffix = line[close + 1..].trim();
            let mut is_template = false;
            let mut parents = Vec::new();
            if !suffix.is_empty() {
                let Some(specification) = suffix
                    .strip_prefix('(')
                    .and_then(|suffix| suffix.strip_suffix(')'))
                else {
                    return Err(ConfigError::Syntax {
                        line: line_number,
                        message: "malformed inheritance list".into(),
                    });
                };
                let mut inherited_names = HashSet::new();
                for entry in specification.split(',') {
                    let entry = entry.trim();
                    if entry.is_empty() {
                        return Err(ConfigError::Syntax {
                            line: line_number,
                            message: "empty inheritance entry".into(),
                        });
                    }
                    if entry == "!" {
                        if is_template {
                            return Err(ConfigError::Syntax {
                                line: line_number,
                                message: "duplicate template marker".into(),
                            });
                        }
                        is_template = true;
                    } else {
                        let canonical = entry.to_ascii_lowercase();
                        if !inherited_names.insert(canonical) {
                            return Err(ConfigError::Syntax {
                                line: line_number,
                                message: format!("duplicate parent template [{entry}]"),
                            });
                        }
                        parents.push(entry.to_owned());
                    }
                }
            }
            if let Some(section) = current.take() {
                sections.push(section);
            }
            let canonical = name.to_ascii_lowercase();
            if !names.insert(canonical) {
                return Err(ConfigError::DuplicateSection(name));
            }
            current = Some(RawSection {
                name,
                line: line_number,
                is_template,
                parents,
                values: Vec::new(),
            });
            continue;
        }
        let Some(section) = current.as_mut() else {
            return Err(ConfigError::Syntax {
                line: line_number,
                message: "setting appears before a section".into(),
            });
        };
        let Some((key, value)) = line.split_once('=') else {
            return Err(ConfigError::Syntax {
                line: line_number,
                message: "expected key = value".into(),
            });
        };
        section.values.push(RawValue {
            key: key.trim().to_owned(),
            value: unquote(value.trim()),
            line: line_number,
            section: section.name.clone(),
        });
    }
    if let Some(section) = current {
        sections.push(section);
    }
    Ok(sections)
}

fn apply_config_overlays(
    sections: &mut Vec<RawSection>,
    overlays: &[ConfigOverlaySection],
) -> Result<(), ConfigError> {
    for overlay in overlays {
        if overlay.name.trim().is_empty() {
            return Err(ConfigError::Syntax {
                line: overlay.line,
                message: format!("{} has an empty section name", overlay.source),
            });
        }
        if overlay.delete {
            sections.retain(|section| !section.name.eq_ignore_ascii_case(&overlay.name));
            continue;
        }

        let index = sections
            .iter()
            .position(|section| section.name.eq_ignore_ascii_case(&overlay.name));
        let index = if let Some(index) = index {
            index
        } else {
            sections.push(RawSection {
                name: overlay.name.clone(),
                line: overlay.line,
                is_template: false,
                parents: Vec::new(),
                values: Vec::new(),
            });
            sections.len() - 1
        };
        let section = &mut sections[index];
        if !overlay.parents.is_empty() {
            section.parents.clone_from(&overlay.parents);
        }
        let kind = overlay.kind.map(|kind| match kind {
            ConfigOverlayKind::Device => TemplateKind::Device,
            ConfigOverlayKind::Line => TemplateKind::Line,
        });
        let mut values = overlay.values.clone();
        if let Some(kind) = kind {
            values.insert(
                0,
                ConfigOverlayValue {
                    key: "type".into(),
                    value: Some(kind.as_str().into()),
                },
            );
        }

        let mut replaced = HashSet::new();
        for value in values {
            let identity = overlay_option_identity(kind, &value.key);
            if replaced.insert(identity.clone()) {
                section
                    .values
                    .retain(|candidate| overlay_option_identity(kind, &candidate.key) != identity);
            }
            if let Some(raw) = value.value {
                section.values.push(RawValue {
                    key: value.key.trim().to_ascii_lowercase(),
                    value: raw,
                    line: overlay.line,
                    section: overlay.source.clone(),
                });
            }
        }
    }
    Ok(())
}

fn overlay_option_identity(kind: Option<TemplateKind>, key: &str) -> String {
    let normalized = normalize_name(key);
    if let Some(kind) = kind {
        return inheritance::option_identity(kind, &normalized);
    }
    match normalized.as_str() {
        "clearbind" => "bind".into(),
        "clearbindaddr" => "bindaddr".into(),
        "clearport" => "port".into(),
        "advertisedaddressipv4" => "advertisedipv4".into(),
        "advertisedaddressipv6" => "advertisedipv6".into(),
        "securebind" => "tlsbind".into(),
        "tlsbindaddr" => "secbindaddr".into(),
        "tlsport" => "secport".into(),
        "tlscombinedpem" => "certfile".into(),
        "tlscertificatefile" => "tlscertificate".into(),
        "tlsprivatekeyfile" => "tlsprivatekey".into(),
        "tlscafile" => "tlstruststore".into(),
        "externaladdress" => "externip".into(),
        "externalhost" => "externhost".into(),
        "externalrefresh" => "externrefresh".into(),
        "signalingtos" | "sccpdscp" | "signalingdscp" => "sccptos".into(),
        "signalingcos" => "sccpcos".into(),
        "audiodscp" => "audiotos".into(),
        "videodscp" => "videotos".into(),
        _ => normalized,
    }
}

fn internal_networks() -> Vec<IpNetwork> {
    vec![
        IpNetwork {
            address: "10.0.0.0".parse().expect("constant IPv4 address"),
            prefix: 8,
        },
        IpNetwork {
            address: "172.16.0.0".parse().expect("constant IPv4 address"),
            prefix: 12,
        },
        IpNetwork {
            address: "192.168.0.0".parse().expect("constant IPv4 address"),
            prefix: 16,
        },
    ]
}

fn invalid_option(
    key: impl Into<String>,
    raw: &str,
    expected: &str,
    sensitive: bool,
) -> ConfigError {
    let found = if sensitive { "<redacted>" } else { raw };
    ConfigError::InvalidValue {
        key: key.into(),
        value: format!("{found:?}; expected {expected}"),
    }
}

fn locate_section_error(error: ConfigError, section: &RawSection) -> ConfigError {
    let ConfigError::InvalidValue { key, mut value } = error else {
        return error;
    };
    if key.starts_with("line ") {
        return ConfigError::InvalidValue { key, value };
    }

    let key_parts: Vec<_> = key.split('.').map(normalize_name).collect();
    let source = section.values.iter().rev().find(|entry| {
        key_parts.contains(&normalize_name(&entry.key))
            || key_parts.iter().any(|part| {
                part == "codecs"
                    && matches!(normalize_name(&entry.key).as_str(), "allow" | "disallow")
            })
    });
    let located_key = source.map_or_else(
        || format!("{}.{}", section.section_location(), key),
        RawValue::diagnostic_key,
    );
    if !value.contains("expected") {
        value.push_str("; expected a valid value for this setting");
    }
    ConfigError::InvalidValue {
        key: located_key,
        value,
    }
}

fn parse_ip_networks(key: &str, raw: &str) -> Result<Vec<IpNetwork>, ConfigError> {
    let raw = raw.trim();
    if raw.eq_ignore_ascii_case("internal") {
        return Ok(internal_networks());
    }
    let (address, mask) = raw.split_once('/').ok_or_else(|| {
        invalid_option(
            key,
            raw,
            "internal or an IPv4/IPv6 network in address/prefix form",
            false,
        )
    })?;
    let address: IpAddr = address.trim().parse().map_err(|_| {
        invalid_option(
            key,
            raw,
            "internal or an IPv4/IPv6 network in address/prefix form",
            false,
        )
    })?;
    let prefix = match address {
        IpAddr::V4(_) => mask
            .trim()
            .parse::<u8>()
            .ok()
            .filter(|prefix| *prefix <= 32)
            .or_else(|| {
                let mask = mask.trim().parse::<Ipv4Addr>().ok()?;
                let bits = u32::from(mask);
                let prefix = bits.leading_ones() as u8;
                (bits == u32::MAX.checked_shl(u32::from(32 - prefix)).unwrap_or(0))
                    .then_some(prefix)
            }),
        IpAddr::V6(_) => mask
            .trim()
            .parse::<u8>()
            .ok()
            .filter(|prefix| *prefix <= 128),
    }
    .ok_or_else(|| {
        invalid_option(
            key,
            raw,
            "a contiguous IPv4 netmask/prefix 0..32 or IPv6 prefix 0..128",
            false,
        )
    })?;
    let address = match address {
        IpAddr::V4(address) => {
            let mask = if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - prefix)
            };
            IpAddr::V4(Ipv4Addr::from(u32::from(address) & mask))
        }
        IpAddr::V6(address) => {
            let mask = if prefix == 0 {
                0
            } else {
                u128::MAX << (128 - prefix)
            };
            IpAddr::V6(Ipv6Addr::from(u128::from(address) & mask))
        }
    };
    Ok(vec![IpNetwork { address, prefix }])
}

fn apply_acl_entry(
    rules: &mut Vec<AclRule>,
    action: AclAction,
    key: &str,
    raw: &str,
) -> Result<(), ConfigError> {
    if raw.trim().is_empty() {
        rules.clear();
        return Ok(());
    }
    rules.extend(
        parse_ip_networks(key, raw)?
            .into_iter()
            .map(|network| AclRule { action, network }),
    );
    Ok(())
}

fn parse_nat_mode(key: &str, raw: &str) -> Result<NatMode, ConfigError> {
    match normalize_name(raw).as_str() {
        "auto" => Ok(NatMode::Auto),
        "off" => Ok(NatMode::Off),
        "autooff" => Ok(NatMode::AutoOff),
        "on" => Ok(NatMode::On),
        "autoon" => Ok(NatMode::AutoOn),
        _ => Err(invalid_option(
            key,
            raw,
            "auto, off, (auto)off, on, or (auto)on",
            false,
        )),
    }
}

fn parse_dscp(key: &str, raw: &str) -> Result<Dscp, ConfigError> {
    let normalized = normalize_name(raw);
    let named = match normalized.as_str() {
        "none" => Some(0),
        "ef" => Some(46),
        "lowdelay" => Some(4),
        "throughput" => Some(2),
        "reliability" => Some(1),
        "mincost" => Some(0),
        value if value.len() == 3 && value.starts_with("cs") => value[2..]
            .parse::<u8>()
            .ok()
            .filter(|class| *class <= 7)
            .map(|class| class * 8),
        value if value.len() == 4 && value.starts_with("af") => {
            let class = value[2..3].parse::<u8>().ok();
            let drop = value[3..].parse::<u8>().ok();
            match (class, drop) {
                (Some(class @ 1..=4), Some(drop @ 1..=3)) => Some(class * 8 + drop * 2),
                _ => None,
            }
        }
        _ => None,
    };
    let value = named.or_else(|| raw.trim().parse::<u8>().ok());
    value.filter(|value| *value <= 63).map(Dscp).ok_or_else(|| {
        invalid_option(
            key,
            raw,
            "DSCP 0..63, CS0..CS7, AF11..AF43, EF, or none",
            false,
        )
    })
}

fn parse_tos_as_dscp(key: &str, raw: &str) -> Result<Dscp, ConfigError> {
    if let Ok(dscp) = parse_dscp(key, raw) {
        return Ok(dscp);
    }
    let trimmed = raw.trim();
    let value = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .map(|hex| u8::from_str_radix(hex, 16))
        .unwrap_or_else(|| trimmed.parse::<u8>())
        .map_err(|_| {
            invalid_option(key, raw, "TOS byte 0..255/0x00..0xff or a DSCP name", false)
        })?;
    Ok(Dscp(value >> 2))
}

fn parse_cos(key: &str, raw: &str) -> Result<Cos, ConfigError> {
    raw.trim()
        .parse::<u8>()
        .ok()
        .filter(|value| *value <= 7)
        .map(Cos)
        .ok_or_else(|| invalid_option(key, raw, "COS priority 0..7", false))
}

fn parse_transport_requirement(key: &str, raw: &str) -> Result<TransportRequirement, ConfigError> {
    match normalize_name(raw).as_str() {
        "clear" | "tcp" => Ok(TransportRequirement::Clear),
        "tls" | "secure" => Ok(TransportRequirement::Tls),
        "either" | "any" => Ok(TransportRequirement::Either),
        _ => Err(invalid_option(key, raw, "clear, tls, or either", false)),
    }
}

fn parse_path(key: &str, raw: &str, sensitive: bool) -> Result<PathBuf, ConfigError> {
    let value = raw.trim();
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(invalid_option(
            key,
            raw,
            "a non-empty filesystem path without control characters",
            sensitive,
        ));
    }
    Ok(PathBuf::from(value))
}

fn parse_hostname(key: &str, raw: &str) -> Result<String, ConfigError> {
    let value = raw.trim();
    if value.is_empty()
        || value.len() > 253
        || value.starts_with('.')
        || value.ends_with('.')
        || value.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        return Err(invalid_option(
            key,
            raw,
            "a valid DNS hostname up to 253 bytes",
            false,
        ));
    }
    Ok(value.to_ascii_lowercase())
}

fn parse_realtime_family(key: &str, raw: &str) -> Result<String, ConfigError> {
    let family = raw.trim();
    if family.is_empty()
        || family.len() > MAX_REALTIME_FAMILY_BYTES
        || !family
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(invalid_option(
            key,
            raw,
            "a non-empty realtime family name up to 45 bytes using letters, digits, or underscore",
            false,
        ));
    }
    Ok(family.into())
}

fn parse<T: FromStr>(key: &str, raw: &str) -> Result<T, ConfigError> {
    raw.parse()
        .map_err(|_| invalid_option(key, raw, std::any::type_name::<T>(), false))
}

fn set_once<T>(
    setting: &mut Option<T>,
    section: &RawSection,
    key: &str,
    raw: &str,
    value: T,
) -> Result<(), ConfigError> {
    SectionValues::new(section).set_once(setting, key, raw, value)
}

fn parse_required_setting(key: &str, raw: &str) -> Result<String, ConfigError> {
    let value = raw.trim();
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(invalid_option(
            key,
            raw,
            "a nonempty printable value",
            false,
        ));
    }
    Ok(value.into())
}

fn parse_metadata_required(
    key: &str,
    raw: &str,
    max_bytes: usize,
    sensitive: bool,
) -> Result<String, ConfigError> {
    let value = raw.trim();
    if value.is_empty()
        || value.len() > max_bytes
        || value
            .chars()
            .any(|character| character == '\0' || character.is_control())
    {
        return Err(invalid_option(
            key,
            raw,
            &format!("a nonempty printable value of at most {max_bytes} bytes"),
            sensitive,
        ));
    }
    Ok(value.into())
}

fn parse_metadata_optional(
    key: &str,
    raw: &str,
    max_bytes: usize,
    sensitive: bool,
) -> Result<Option<String>, ConfigError> {
    if raw.trim().is_empty() {
        return Ok(None);
    }
    parse_metadata_required(key, raw, max_bytes, sensitive).map(Some)
}

fn push_channel_variable(
    variables: &mut Vec<ChannelVariable>,
    key: &str,
    raw: &str,
) -> Result<(), ConfigError> {
    let invalid = || {
        invalid_option(
            key,
            raw,
            "a unique, nonsensitive NAME=value assignment within channel-variable bounds",
            true,
        )
    };
    let (name, value) = raw.split_once('=').ok_or_else(invalid)?;
    let name = name.trim();
    let value = value.trim();
    if value.is_empty() {
        return Err(invalid());
    }
    let variable = ChannelVariable::new(name, value).map_err(|_| invalid())?;
    if variables.len() >= MAX_VARIABLES
        || variables
            .iter()
            .any(|configured| configured.name() == variable.name())
    {
        return Err(invalid());
    }
    let aggregate = variables
        .iter()
        .map(|configured| configured.name().len() + configured.value().len())
        .sum::<usize>()
        .checked_add(variable.name().len() + variable.value().len())
        .ok_or_else(invalid)?;
    if aggregate > MAX_VARIABLE_AGGREGATE_BYTES {
        return Err(invalid());
    }
    variables.push(variable);
    Ok(())
}

fn parse_optional_setting(key: &str, raw: &str) -> Result<Option<String>, ConfigError> {
    let value = raw.trim();
    if value.is_empty()
        || matches!(
            value.to_ascii_lowercase().as_str(),
            "none" | "off" | "disabled"
        )
    {
        return Ok(None);
    }
    parse_required_setting(key, value).map(Some)
}

fn parse_optional_voicemail_destination(
    key: &str,
    raw: &str,
) -> Result<Option<VoicemailDestination>, ConfigError> {
    let value = raw.trim();
    if value.is_empty()
        || matches!(
            value.to_ascii_lowercase().as_str(),
            "none" | "off" | "disabled"
        )
    {
        return Ok(None);
    }
    VoicemailDestination::new(value)
        .map(Some)
        .map_err(|_| invalid_option(key, "<redacted>", "a bounded printable destination", true))
}

fn parse_optional_forwarding_destination(
    key: &str,
    raw: &str,
) -> Result<Option<ForwardingDestination>, ConfigError> {
    let value = raw.trim();
    if value.is_empty()
        || matches!(
            value.to_ascii_lowercase().as_str(),
            "none" | "off" | "disabled"
        )
    {
        return Ok(None);
    }
    ForwardingDestination::new(value)
        .map(Some)
        .map_err(|_| invalid_option(key, "<redacted>", "a bounded printable destination", true))
}

fn parse_optional_hotline_destination(
    key: &str,
    raw: &str,
) -> Result<Option<HotlineDestination>, ConfigError> {
    let value = raw.trim();
    if value.is_empty() {
        return Ok(None);
    }
    HotlineDestination::new(value)
        .map(Some)
        .map_err(|_| invalid_option(key, "<redacted>", "a bounded printable destination", true))
}

fn parse_empty_optional_setting(key: &str, raw: &str) -> Result<Option<String>, ConfigError> {
    let value = raw.trim();
    if value.is_empty() {
        return Ok(None);
    }
    parse_required_setting(key, value).map(Some)
}

fn parse_setting_allow_empty(key: &str, raw: &str) -> Result<String, ConfigError> {
    let value = raw.trim();
    if value.chars().any(char::is_control) {
        return Err(invalid_option(key, raw, "a printable value", false));
    }
    Ok(value.into())
}

fn parse_bounded_setting_allow_empty(
    key: &str,
    raw: &str,
    max_bytes: usize,
) -> Result<String, ConfigError> {
    let value = parse_setting_allow_empty(key, raw)?;
    if value.len() > max_bytes {
        return Err(invalid_option(
            key,
            raw,
            &format!("at most {max_bytes} bytes"),
            false,
        ));
    }
    Ok(value)
}

fn parse_application_options(key: &str, raw: &str) -> Result<String, ConfigError> {
    let value = raw.trim();
    if value.chars().any(char::is_control) {
        return Err(invalid_option(
            key,
            raw,
            "printable application options",
            false,
        ));
    }
    Ok(value.into())
}

fn parse_parking_lot_button(
    key: &str,
    raw: Option<&str>,
) -> Result<ParkingLotButtonConfig, ConfigError> {
    let fields: Vec<_> = raw
        .unwrap_or("default,RetrieveSingle")
        .split(',')
        .map(str::trim)
        .collect();
    if !(1..=2).contains(&fields.len()) || fields[0].is_empty() {
        return Err(ConfigError::InvalidValue {
            key: key.into(),
            value: raw.unwrap_or_default().into(),
        });
    }
    let retrieval = if fields.len() == 1 {
        ParkingRetrievalBehavior::RetrieveSingle
    } else {
        match normalize_name(fields[1]).as_str() {
            "retrievesingle" => ParkingRetrievalBehavior::RetrieveSingle,
            "alwaysshowmenu" => ParkingRetrievalBehavior::AlwaysShowMenu,
            _ => {
                return Err(ConfigError::InvalidValue {
                    key: key.into(),
                    value: raw.unwrap_or_default().into(),
                });
            }
        }
    };
    Ok(ParkingLotButtonConfig {
        lot: parse_required_setting(key, fields[0])?,
        retrieval,
    })
}

fn parse_mailbox(key: &str, raw: &str) -> Result<Option<String>, ConfigError> {
    let Some(mailbox) = parse_optional_setting(key, raw)? else {
        return Ok(None);
    };
    let mut parts = mailbox.split('@');
    let name = parts.next().unwrap_or_default();
    let context = parts.next();
    if name.is_empty()
        || name.chars().any(char::is_whitespace)
        || context
            .is_some_and(|context| context.is_empty() || context.chars().any(char::is_whitespace))
        || parts.next().is_some()
    {
        return Err(invalid_option(
            key,
            raw,
            "mailbox or mailbox@context without whitespace",
            false,
        ));
    }
    Ok(Some(mailbox))
}

fn parse_mobility_pin(key: &str, raw: &str) -> Result<Option<MobilityPin>, ConfigError> {
    let value = raw.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > MAX_MOBILITY_PIN_DIGITS || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid_option(
            key,
            raw,
            "one to seven ASCII digits, or empty to disable mobility login",
            true,
        ));
    }
    Ok(Some(MobilityPin(value.into())))
}

fn validate_registration_identifier(
    key: &str,
    value: &str,
    expected: &str,
) -> Result<(), ConfigError> {
    if value.is_empty()
        || value.len() > MAX_REGISTRATION_IDENTIFIER_BYTES
        || value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
        || value.contains(['&', '@'])
    {
        return Err(invalid_option(key, value, expected, false));
    }
    Ok(())
}

fn parse_registration_contexts(key: &str, raw: &str) -> Result<Vec<String>, ConfigError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    if raw.len() > MAX_REGISTRATION_IDENTIFIER_BYTES {
        return Err(invalid_option(
            key,
            raw,
            "an ampersand-separated context list totaling at most 79 bytes",
            false,
        ));
    }
    let mut contexts = Vec::new();
    let mut seen = HashSet::new();
    for field in raw.split('&') {
        let context = field.trim();
        validate_registration_identifier(
            key,
            context,
            "unique, nonempty context names without whitespace, ampersands, or @",
        )?;
        if !seen.insert(context.to_owned()) {
            return Err(invalid_option(
                key,
                raw,
                "unique ampersand-separated context names",
                false,
            ));
        }
        contexts.push(context.to_owned());
    }
    Ok(contexts)
}

fn parse_registration_extensions(
    key: &str,
    raw: &str,
) -> Result<Option<Vec<RegistrationExtension>>, ConfigError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    if raw.len() > MAX_REGISTRATION_EXTENSION_LIST_BYTES {
        return Err(invalid_option(
            key,
            raw,
            "an ampersand-separated extension list totaling at most 255 bytes",
            false,
        ));
    }
    let mut extensions = Vec::new();
    let mut seen = HashSet::new();
    for field in raw.split('&') {
        let field = field.trim();
        let (extension, context) = if let Some((extension, context)) = field.split_once('@') {
            if context.contains('@') {
                return Err(invalid_option(
                    key,
                    raw,
                    "extension or extension@context entries separated by ampersands",
                    false,
                ));
            }
            (extension.trim(), Some(context.trim()))
        } else {
            (field, None)
        };
        validate_registration_identifier(
            key,
            extension,
            "a nonempty registration extension up to 79 bytes without whitespace, ampersands, or @",
        )?;
        if let Some(context) = context {
            validate_registration_identifier(
                key,
                context,
                "a nonempty registration context up to 79 bytes without whitespace, ampersands, or @",
            )?;
        }
        let entry = RegistrationExtension {
            extension: extension.into(),
            context: context.map(str::to_owned),
        };
        if !seen.insert(entry.clone()) {
            return Err(invalid_option(
                key,
                raw,
                "unique extension or extension@context entries",
                false,
            ));
        }
        extensions.push(entry);
    }
    Ok(Some(extensions))
}

fn resolve_registration_targets(
    contexts: &[String],
    extensions: &[RegistrationExtension],
) -> Vec<RegistrationTarget> {
    extensions
        .iter()
        .flat_map(|entry| {
            if let Some(context) = &entry.context {
                vec![RegistrationTarget {
                    extension: entry.extension.clone(),
                    context: context.clone(),
                }]
            } else {
                contexts
                    .iter()
                    .map(|context| RegistrationTarget {
                        extension: entry.extension.clone(),
                        context: context.clone(),
                    })
                    .collect()
            }
        })
        .collect()
}

fn parse_dnd_mode(key: &str, raw: &str) -> Result<DndMode, ConfigError> {
    match normalize_name(raw).as_str() {
        "off" | "none" | "disabled" => Ok(DndMode::Off),
        "silent" => Ok(DndMode::Silent),
        "reject" | "busy" => Ok(DndMode::Reject),
        _ => Err(invalid_option(key, raw, "off, silent, or reject", false)),
    }
}

fn parse_dnd_button_mode(key: &str, raw: &str) -> Result<DndButtonMode, ConfigError> {
    match normalize_name(raw).as_str() {
        "silent" => Ok(DndButtonMode::Silent),
        "reject" | "busy" => Ok(DndButtonMode::Reject),
        _ => Err(invalid_option(key, raw, "silent or reject", false)),
    }
}

fn parse_feature_default(key: &str, raw: &str) -> Result<(u32, bool), ConfigError> {
    let fields: Vec<_> = raw.split(',').map(str::trim).collect();
    if fields.len() != 2 {
        return Err(invalid_option(
            key,
            raw,
            "feature instance and boolean: instance,yes|no",
            false,
        ));
    }
    let instance = parse::<u32>(key, fields[0])?;
    if instance == 0 {
        return Err(invalid_option(
            key,
            raw,
            "feature instance >= 1 and boolean: instance,yes|no",
            false,
        ));
    }
    Ok((instance, parse_bool(key, fields[1])?))
}

fn parse_numeric_groups(key: &str, raw: &str) -> Result<BTreeSet<u8>, ConfigError> {
    let mut groups = BTreeSet::new();
    if raw.trim().is_empty() {
        return Ok(groups);
    }
    for field in raw.split(',') {
        let field = field.trim();
        if field.is_empty() {
            return Err(invalid_option(
                key,
                raw,
                "comma-separated groups or ranges in 0..63",
                false,
            ));
        }
        let (start, end) = if let Some((start, end)) = field.split_once('-') {
            if end.contains('-') {
                return Err(invalid_option(
                    key,
                    raw,
                    "comma-separated groups or ranges in 0..63",
                    false,
                ));
            }
            (
                parse::<u8>(key, start.trim())?,
                parse::<u8>(key, end.trim())?,
            )
        } else {
            let value = parse::<u8>(key, field)?;
            (value, value)
        };
        if start > end || end > 63 {
            return Err(invalid_option(
                key,
                raw,
                "ascending group values or ranges in 0..63",
                false,
            ));
        }
        for group in start..=end {
            if !groups.insert(group) {
                return Err(invalid_option(
                    key,
                    raw,
                    "unique group values in 0..63",
                    false,
                ));
            }
        }
    }
    Ok(groups)
}

fn parse_named_groups(key: &str, raw: &str) -> Result<BTreeSet<String>, ConfigError> {
    let mut groups = BTreeSet::new();
    if raw.trim().is_empty() {
        return Ok(groups);
    }
    for field in raw.split(',') {
        let group = field.trim();
        if group.is_empty()
            || group.chars().any(char::is_control)
            || !groups.insert(group.to_owned())
        {
            return Err(invalid_option(
                key,
                raw,
                "unique, nonempty named groups",
                false,
            ));
        }
    }
    Ok(groups)
}

fn parse_bool(key: &str, raw: &str) -> Result<bool, ConfigError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        _ => Err(invalid_option(key, raw, "yes or no", false)),
    }
}

fn parse_dial_terminator(key: &str, raw: &str) -> Result<char, ConfigError> {
    let value = raw.trim();
    let mut characters = value.chars();
    let Some(character) = characters.next() else {
        return Err(invalid_option(key, raw, "one DTMF character", false));
    };
    if characters.next().is_some() {
        return Err(invalid_option(key, raw, "one DTMF character", false));
    }
    let character = character.to_ascii_uppercase();
    if !matches!(character, '0'..='9' | '*' | '#' | 'A'..='D') {
        return Err(invalid_option(
            key,
            raw,
            "one DTMF character: 0..9, *, #, or A..D",
            false,
        ));
    }
    Ok(character)
}

fn parse_secondary_dialtone_digits(key: &str, raw: &str) -> Result<Option<String>, ConfigError> {
    let value = raw.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > 9
        || !value
            .chars()
            .all(|character| matches!(character, '0'..='9' | '*' | '#' | 'A'..='D' | 'a'..='d'))
    {
        return Err(invalid_option(
            key,
            raw,
            "up to 9 DTMF characters: 0..9, *, #, or A..D",
            false,
        ));
    }
    Ok(Some(value.to_ascii_uppercase()))
}

fn parse_call_answer_order(key: &str, raw: &str) -> Result<CallAnswerOrder, ConfigError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "oldestfirst" => Ok(CallAnswerOrder::OldestFirst),
        "lastfirst" => Ok(CallAnswerOrder::LastFirst),
        _ => Err(invalid_option(key, raw, "OldestFirst or LastFirst", false)),
    }
}

fn parse_fallback_decision(key: &str, raw: &str) -> Result<FallbackDecision, ConfigError> {
    let value = raw.trim();
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(FallbackDecision::Accept),
        "false" | "no" | "off" | "0" => Ok(FallbackDecision::Reject),
        "odd" => Ok(FallbackDecision::DeviceIdOdd),
        "even" => Ok(FallbackDecision::DeviceIdEven),
        _ => Err(invalid_option(
            key,
            raw,
            "yes, no, odd, or even",
            value.contains('/') || value.contains('\\'),
        )),
    }
}

fn parse_signaling_server(key: &str, raw: &str) -> Result<SignalingServerRoute, ConfigError> {
    let fields = raw.split(',').map(str::trim).collect::<Vec<_>>();
    let invalid = || {
        invalid_option(
            key,
            raw,
            "priority,name,address,clear-port-or-none,secure-port-or-none",
            false,
        )
    };
    let [priority, name, address, clear_port, secure_port] = fields.as_slice() else {
        return Err(invalid());
    };
    let priority = priority
        .parse::<u8>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(invalid)?;
    if name.is_empty() || name.len() >= 48 || name.chars().any(char::is_control) {
        return Err(invalid());
    }
    let address = address
        .parse::<IpAddr>()
        .ok()
        .filter(|address| !address.is_unspecified() && !address.is_multicast())
        .ok_or_else(invalid)?;
    let port = |value: &str| {
        if matches!(normalize_name(value).as_str(), "none" | "off" | "disabled") {
            Some(None)
        } else {
            value
                .parse::<u16>()
                .ok()
                .and_then(std::num::NonZeroU16::new)
                .map(Some)
        }
    };
    let clear_port = port(clear_port).ok_or_else(invalid)?;
    let secure_port = port(secure_port).ok_or_else(invalid)?;
    if clear_port.is_none() && secure_port.is_none() {
        return Err(invalid());
    }
    Ok(SignalingServerRoute {
        priority,
        name: (*name).into(),
        address,
        clear_port,
        secure_port,
    })
}

fn parse_early_media(key: &str, raw: &str) -> Result<bool, ConfigError> {
    match normalize_name(raw).as_str() {
        "yes" | "true" | "on" | "1" => Ok(true),
        "no" | "false" | "off" | "0" | "none" => Ok(false),
        // Accepted compatibility values all map to enabled early media.
        "offhook" | "immediate" | "dial" | "ringout" | "progress" => Ok(true),
        _ => Err(invalid_option(
            key,
            raw,
            "yes, no, none, offhook, immediate, dial, ringout, or progress",
            false,
        )),
    }
}

fn parse_dtmf_mode(key: &str, raw: &str) -> Result<DtmfMode, ConfigError> {
    match normalize_name(raw).as_str() {
        "auto" => Ok(DtmfMode::Auto),
        "rfc2833" => Ok(DtmfMode::Rfc2833),
        "skinny" => Ok(DtmfMode::Skinny),
        _ => Err(invalid_option(key, raw, "auto, rfc2833, or skinny", false)),
    }
}

fn parse_video_mode(key: &str, raw: &str) -> Result<VideoMode, ConfigError> {
    match normalize_name(raw).as_str() {
        "off" => Ok(VideoMode::Off),
        "user" => Ok(VideoMode::User),
        "auto" => Ok(VideoMode::Auto),
        _ => Err(invalid_option(key, raw, "off, user, or auto", false)),
    }
}

fn parse_media_encryption_policy(
    key: &str,
    raw: &str,
) -> Result<MediaEncryptionPolicy, ConfigError> {
    let mut fields = raw.split(',').map(str::trim);
    let requirement = match fields.next().map(normalize_name).as_deref() {
        Some("off") => MediaEncryptionRequirement::Off,
        Some("optional") => MediaEncryptionRequirement::Optional,
        Some("required") => MediaEncryptionRequirement::Required,
        _ => {
            return Err(invalid_option(
                key,
                raw,
                "off, optional,<profile...>, or required,<profile...>",
                false,
            ));
        }
    };
    let profiles = fields
        .map(|profile| {
            profile.parse::<MediaEncryptionProfile>().map_err(|_| {
                invalid_option(key, raw, "a canonical media-encryption profile list", false)
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    MediaEncryptionPolicy::new(requirement, profiles).map_err(|_| {
        invalid_option(
            key,
            raw,
            "off without profiles, or optional/required with at least one profile",
            false,
        )
    })
}

fn parse_jitter_buffer_implementation(
    key: &str,
    raw: &str,
) -> Result<JitterBufferImplementation, ConfigError> {
    match normalize_name(raw).as_str() {
        "fixed" => Ok(JitterBufferImplementation::Fixed),
        "adaptive" => Ok(JitterBufferImplementation::Adaptive),
        _ => Err(invalid_option(key, raw, "fixed or adaptive", false)),
    }
}

fn parse_positive_jitter_millis(key: &str, raw: &str) -> Result<u32, ConfigError> {
    let value = raw.trim().parse::<u32>().map_err(|_| {
        invalid_option(
            key,
            raw,
            "a positive millisecond value no greater than 2147483647",
            false,
        )
    })?;
    if value == 0 || value > i32::MAX as u32 {
        return Err(invalid_option(
            key,
            raw,
            "a positive millisecond value no greater than 2147483647",
            false,
        ));
    }
    Ok(value)
}

fn parse_ringer_mode(key: &str, raw: &str) -> Result<RingerMode, ConfigError> {
    match normalize_name(raw).as_str() {
        "off" => Ok(RingerMode::Off),
        "inside" => Ok(RingerMode::Inside),
        "outside" => Ok(RingerMode::Outside),
        "feature" => Ok(RingerMode::Feature),
        "silent" => Ok(RingerMode::Silent),
        "urgent" => Ok(RingerMode::Urgent),
        "bellcore1" => Ok(RingerMode::Bellcore1),
        "bellcore2" => Ok(RingerMode::Bellcore2),
        "bellcore3" => Ok(RingerMode::Bellcore3),
        "bellcore4" => Ok(RingerMode::Bellcore4),
        "bellcore5" => Ok(RingerMode::Bellcore5),
        _ => Err(invalid_option(
            key,
            raw,
            "Off, Inside, Outside, Feature, Silent, Urgent, or Bellcore1..Bellcore5",
            false,
        )),
    }
}

fn parse_tone(key: &str, raw: &str) -> Result<Tone, ConfigError> {
    let trimmed = raw.trim();
    let numeric = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .map(|hex| u32::from_str_radix(hex, 16))
        .unwrap_or_else(|| trimmed.parse::<u32>());
    if let Ok(value) = numeric {
        if value <= u8::MAX.into() {
            return Ok(Tone::from(value));
        }
        return Err(ConfigError::InvalidValue {
            key: key.into(),
            value: raw.into(),
        });
    }

    let tone = match normalize_name(trimmed).as_str() {
        "silence" => Tone::Silence,
        "dtmf1" => Tone::Dtmf1,
        "dtmf2" => Tone::Dtmf2,
        "dtmf3" => Tone::Dtmf3,
        "dtmf4" => Tone::Dtmf4,
        "dtmf5" => Tone::Dtmf5,
        "dtmf6" => Tone::Dtmf6,
        "dtmf7" => Tone::Dtmf7,
        "dtmf8" => Tone::Dtmf8,
        "dtmf9" => Tone::Dtmf9,
        "dtmf0" => Tone::Dtmf0,
        "dtmfstar" => Tone::DtmfStar,
        "dtmfpound" => Tone::DtmfPound,
        "dtmfa" => Tone::DtmfA,
        "dtmfb" => Tone::DtmfB,
        "dtmfc" => Tone::DtmfC,
        "dtmfd" => Tone::DtmfD,
        "insidedial" | "insidedialtone" => Tone::InsideDial,
        "outsidedial" | "outsidedialtone" => Tone::OutsideDial,
        "linebusy" | "linebusytone" => Tone::LineBusy,
        "alerting" | "alertingtone" => Tone::Alerting,
        "reorder" | "reordertone" => Tone::Reorder,
        "recorderwarning" | "recorderwarningtone" => Tone::RecorderWarning,
        "recorderdetected" | "recorderdetectedtone" => Tone::RecorderDetected,
        "reverting" | "revertingtone" => Tone::Reverting,
        "receiveroffhook" | "receiveroffhooktone" => Tone::ReceiverOffHook,
        "partialdial" | "partialdialtone" => Tone::PartialDial,
        "nosuchnumber" | "nosuchnumbertone" => Tone::NoSuchNumber,
        "busyverification" | "busyverificationtone" => Tone::BusyVerification,
        "callwaiting" | "callwaitingtone" => Tone::CallWaiting,
        "confirmation" | "confirmationtone" => Tone::Confirmation,
        "campon" | "camponindicationtone" => Tone::CampOn,
        "recalldial" | "recalldialtone" => Tone::RecallDial,
        "zipzip" => Tone::ZipZip,
        "zip" => Tone::Zip,
        "beepbonk" => Tone::BeepBonk,
        "music" | "musictone" => Tone::Music,
        "hold" | "holdtone" => Tone::Hold,
        "test" | "testtone" => Tone::Test,
        "monitorwarning" | "dtmonitorwarningtone" => Tone::MonitorWarning,
        "addcallwaiting" => Tone::AddCallWaiting,
        "prioritycallwaiting" | "prioritycallwait" => Tone::PriorityCallWaiting,
        "bargein" | "bargin" => Tone::BargeIn,
        "distinctalert" => Tone::DistinctAlert,
        "priorityalert" => Tone::PriorityAlert,
        "reminderring" => Tone::ReminderRing,
        "precedenceringback" => Tone::PrecedenceRingback,
        "preemption" | "preemptiontone" => Tone::Preemption,
        "notone" => Tone::NoTone,
        "meetmegreeting" | "meetmegreetingtone" => Tone::MeetMeGreeting,
        "meetmenumberinvalid" | "meetmenumberinvalidtone" => Tone::MeetMeNumberInvalid,
        "meetmenumberfailed" | "meetmenumberfailedtone" => Tone::MeetMeNumberFailed,
        "meetmeenterpin" | "meetmeenterpintone" => Tone::MeetMeEnterPin,
        "meetmeinvalidpin" | "meetmeinvalidpintone" => Tone::MeetMeInvalidPin,
        "meetmefailedpin" | "meetmefailedpintone" => Tone::MeetMeFailedPin,
        "meetmecfbfailed" | "meetmecfbfailedtone" => Tone::MeetMeCfbFailed,
        "meetmeenteraccesscode" | "meetmeenteraccesscodetone" => Tone::MeetMeEnterAccessCode,
        "meetmeaccesscodeinvalid" | "meetmeaccesscodeinvalidtone" => Tone::MeetMeAccessCodeInvalid,
        "meetmeaccesscodefailed" | "meetmeaccesscodefailedtone" => Tone::MeetMeAccessCodeFailed,
        _ => {
            return Err(ConfigError::InvalidValue {
                key: key.into(),
                value: raw.into(),
            });
        }
    };
    Ok(tone)
}

fn apply_codec_settings(
    mut codecs: Vec<Codec>,
    settings: &[(bool, &str)],
    key: &str,
) -> Result<Vec<Codec>, ConfigError> {
    for (allow_setting, raw) in settings {
        let tokens = raw.split(',').map(str::trim).collect::<Vec<_>>();
        if tokens.len() > 1 && tokens.iter().any(|token| token.eq_ignore_ascii_case("all")) {
            return Err(ConfigError::InvalidValue {
                key: key.into(),
                value: (*raw).into(),
            });
        }
        for token in tokens {
            let mut token = token.trim();
            if token.is_empty() {
                return Err(ConfigError::InvalidValue {
                    key: key.into(),
                    value: (*raw).into(),
                });
            }
            let mut allow = *allow_setting;
            if let Some(negated) = token.strip_prefix('!') {
                token = negated.trim();
                allow = !allow;
                if token.is_empty() {
                    return Err(ConfigError::InvalidValue {
                        key: key.into(),
                        value: (*raw).into(),
                    });
                }
            }
            if token.eq_ignore_ascii_case("all") && !allow {
                codecs.clear();
                continue;
            }
            let candidates = codec_group(token).ok_or_else(|| ConfigError::InvalidValue {
                key: key.into(),
                value: token.into(),
            })?;
            for codec in candidates {
                codecs.retain(|candidate| candidate != &codec);
                if allow {
                    codecs.push(codec);
                    if codecs.len() > MAX_CODEC_PREFERENCES {
                        return Err(ConfigError::InvalidValue {
                            key: key.into(),
                            value: format!("more than {MAX_CODEC_PREFERENCES} codec preferences"),
                        });
                    }
                }
            }
        }
    }
    if !codecs.iter().any(|codec| codec.kind() == CodecKind::Audio) {
        return Err(ConfigError::InvalidValue {
            key: key.into(),
            value: "at least one audio codec is required".into(),
        });
    }
    if let Some(codec) = codecs
        .iter()
        .copied()
        .find(|codec| matches!(codec.kind(), CodecKind::Audio) && pbx_audio_format(*codec).is_err())
    {
        return Err(ConfigError::InvalidValue {
            key: key.into(),
            value: unsupported_audio_reason(codec)
                .unwrap_or("codec has no Asterisk audio format mapping")
                .into(),
        });
    }
    Ok(codecs)
}

fn mapped_audio_codecs() -> Vec<Codec> {
    vec![
        Codec::Pcmu,
        Codec::G711Ulaw56k,
        Codec::Pcma,
        Codec::G711Alaw56k,
        Codec::G72264k,
        Codec::G72256k,
        Codec::G72248k,
        Codec::G7231,
        Codec::G729,
        Codec::G729A,
        Codec::G729B,
        Codec::G729Ab,
        Codec::G729AnnexB,
        Codec::G726_32k,
        Codec::Gsm,
        Codec::Wideband256k,
        Codec::Ilbc,
        Codec::G7221_32k,
        Codec::Opus,
    ]
}

fn codec_group(raw: &str) -> Option<Vec<Codec>> {
    let codecs = match normalize_name(raw).as_str() {
        "all" => mapped_audio_codecs(),
        "is11172" => vec![Codec::Is11172],
        "is13872" => vec![Codec::Is13818],
        "gsm" => vec![Codec::Gsm],
        "slin16" => vec![Codec::Wideband256k],
        "activevoice" => vec![Codec::ActiveVoice],
        "alaw" => vec![Codec::Pcma, Codec::G711Alaw56k],
        "ulaw" => vec![Codec::Pcmu, Codec::G711Ulaw56k],
        "g722" => vec![Codec::G72264k, Codec::G72256k, Codec::G72248k],
        "g7221" => vec![Codec::G7221_32k],
        "g723" => vec![Codec::G7231],
        "g726" => vec![Codec::G726_32k],
        "g728" => vec![Codec::G728],
        "g729" => vec![
            Codec::G729,
            Codec::G729A,
            Codec::G729B,
            Codec::G729Ab,
            Codec::G729AnnexB,
        ],
        "ilbc" => vec![Codec::Ilbc],
        "isac" => vec![Codec::Isac],
        "opus" => vec![Codec::Opus],
        "h224" => vec![Codec::H224],
        "aac" => vec![Codec::Aac],
        "mp4alatm128" => vec![Codec::Mp4aLatm128],
        "mp4alatm64" => vec![Codec::Mp4aLatm64],
        "mp4alatm56" => vec![Codec::Mp4aLatm56],
        "mp4alatm48" => vec![Codec::Mp4aLatm48],
        "mp4alatm32" => vec![Codec::Mp4aLatm32],
        "mp4alatm24" => vec![Codec::Mp4aLatm24],
        "mp4alatmna" => vec![Codec::Mp4aLatm],
        "amr" => vec![Codec::Amr],
        "amrwb" => vec![Codec::AmrWb],
        "h261" => vec![Codec::H261],
        "h263" => vec![Codec::H263, Codec::H263Plus],
        "h264" => vec![Codec::H264, Codec::H264Svc, Codec::H264Fec, Codec::H264Uc],
        "h265" => vec![Codec::H265],
        "t120" => vec![Codec::T120],
        "data" => vec![Codec::Data64k, Codec::Data56k],
        "t38fax" => vec![Codec::T38Fax],
        "tote" => vec![Codec::Tote],
        "xv711u" => vec![Codec::Xv150ModemRelay711u],
        "v711u" => vec![Codec::NseVbd711u],
        "xv729a" => vec![Codec::Xv150ModemRelay729a],
        "v729a" => vec![Codec::NseVbd729a],
        "clearchan" => vec![Codec::ClearChannel],
        "univxcoder" => vec![Codec::UniversalTranscoder],
        "rfc2833" => vec![Codec::DtmfOutOfBandRfc2833],
        "passthrough" => vec![Codec::DtmfPassthrough],
        "dynamic" => vec![Codec::DtmfDynamic],
        "oob" => vec![Codec::DtmfOutOfBand],
        "rfc2833ib" => vec![Codec::DtmfInBandRfc2833],
        "cfb" => vec![Codec::CfbTones],
        "noaudio" => vec![Codec::DtmfNoAudio],
        "v150modem" => vec![Codec::V150ModemRelay],
        "v150sprt" => vec![Codec::V150Sprt],
        "v150sse" => vec![Codec::V150Sse],
        _ => return None,
    };
    Some(codecs)
}

fn parse_caller_id(raw: &str) -> (String, String) {
    let raw = raw.trim();
    if let Some((name, number)) = raw.rsplit_once('<')
        && let Some(number) = number.strip_suffix('>')
    {
        return (
            name.trim().trim_matches('"').to_owned(),
            number.trim().to_owned(),
        );
    }
    (raw.to_owned(), raw.to_owned())
}

fn value<'a>(section: &'a RawSection, key: &str) -> Option<&'a str> {
    section
        .values
        .iter()
        .rev()
        .find(|value| value.key.eq_ignore_ascii_case(key))
        .map(|value| value.value.as_str())
}

fn strip_inline_comment(line: &str) -> &str {
    let mut quoted = false;
    let mut escaped = false;
    for (index, character) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ';' if !quoted => return &line[..index],
            _ => {}
        }
    }
    line
}

fn unquote(value: &str) -> String {
    let Some(inner) = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    else {
        return value.to_owned();
    };
    let mut output = String::with_capacity(inner.len());
    let mut characters = inner.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            output.push(character);
            continue;
        }
        match characters.next() {
            Some(escaped @ ('\\' | '"')) => output.push(escaped),
            Some(other) => {
                output.push('\\');
                output.push(other);
            }
            None => output.push('\\'),
        }
    }
    output
}

#[cfg(test)]
#[path = "tests/mod.rs"]
mod tests;
