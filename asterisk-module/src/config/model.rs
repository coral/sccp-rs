//! Public normalized configuration models.

use super::*;

#[derive(Clone, Eq, PartialEq)]
pub struct GeneralConfig {
    pub configuration_source: ConfigurationSource,
    pub bind: SocketAddr,
    pub advertised_address: Ipv4Addr,
    pub server_name: String,
    /// Default PBX prompt language inherited by logical lines.
    pub language: String,
    /// Optional default CDR account code inherited by logical lines.
    pub account_code: Option<String>,
    pub keepalive_seconds: u32,
    pub secondary_keepalive_seconds: u32,
    pub signaling_servers: Vec<SignalingServerRoute>,
    pub first_digit_timeout_ms: u64,
    pub interdigit_timeout_ms: u64,
    pub dial_terminator: DialTerminatorConfig,
    pub simulate_enbloc: bool,
    /// Service policy for speed-dial digit collection.
    pub speed_dial_await_further_digits: bool,
    pub allow_overlap: bool,
    /// Complete an eligible in-flight consultation when its handset leg goes
    /// on-hook. The value is captured when the transfer begins.
    pub transfer_on_hangup: bool,
    /// Ordering used when selecting among multiple answerable calls.
    pub call_answer_order: CallAnswerOrder,
    /// Fixed SCCP station wall-clock offset from UTC.
    pub timezone_offset_minutes: i16,
    /// SCCP station date-field order and separator.
    pub date_template: DateTemplate,
    /// Default physical ringer for ordinary inbound presentations.
    pub ring_type: RingerMode,
    /// Tone played on the existing active call when another call arrives.
    pub call_waiting_tone: Option<Tone>,
    /// Repeat interval in seconds; zero disables repeats.
    pub call_waiting_interval_seconds: u32,
    pub codecs: Vec<Codec>,
    pub audio_encryption: MediaEncryptionPolicy,
    /// Defaults for destination-based conference dialing. Device sections may
    /// override these values, and line sections may override them again.
    pub conference_dialing: ConferenceDialingConfig,
    pub auto_answer: AutoAnswerConfig,
    /// Tone played while an active handset presentation briefly reports a
    /// passive remote termination. `None` disables the delayed notification.
    pub remote_hangup_tone: Option<Tone>,
    pub guest_hotline: GuestHotlineConfig,
    pub direct_media: bool,
    pub early_media: bool,
    /// Station-side echo cancellation and silence suppression defaults. Line
    /// sections may override either setting independently.
    pub audio_processing: AudioProcessingPolicy,
    pub jitter_buffer: JitterBufferConfig,
    pub registration: RegistrationConfig,
    /// Policy used when a station registered to another configured server asks
    /// whether it should move back to this server.
    pub fallback_registration: FallbackRegistrationConfig,
    pub network: NetworkPolicy,
    pub qos: QosPolicy,
    pub listeners: ListenerPolicy,
    /// Realtime table families selected for file-plus-realtime configuration.
    /// Both families are required so refreshes always build a complete
    /// device/line candidate before replacing the live snapshot.
    pub realtime_tables: Option<RealtimeTableConfig>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ConfigurationSource {
    #[default]
    File,
    Sorcery,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceConfig {
    pub id: DeviceId,
    pub description: String,
    /// Line names in line-instance order, retained for channel lookups.
    pub lines: Vec<String>,
    /// Physical station buttons in configuration order.
    pub buttons: Vec<ButtonDefinition>,
    /// Optional feature arguments keyed by feature-button instance.
    pub feature_arguments: HashMap<u32, String>,
    /// Asterisk extension hints keyed by BLF feature-button instance.
    pub blf_targets: HashMap<u32, HintTarget>,
    /// Ordered, validated PBX variables applied before logical-line values.
    pub channel_variables: Vec<ChannelVariable>,
    /// Canonical name of the resolved reusable soft-key profile.
    pub soft_key_profile: String,
    /// Initial mutable feature state and feature availability.
    pub feature_defaults: DeviceFeatureDefaults,
    /// Recurring weekly DND policy in configuration order.
    pub dnd_schedules: Vec<DndSchedule>,
    pub parking: DeviceParkingConfig,
    pub conference: DeviceConferenceConfig,
    pub call_ui: DeviceCallUiConfig,
    pub allow_overlap: bool,
    pub media: DeviceMediaConfig,
    pub network: DeviceNetworkPolicy,
}

#[derive(Clone, Eq, PartialEq)]
pub struct LineConfig {
    pub number: String,
    pub label: String,
    pub context: String,
    pub caller_name: String,
    pub caller_number: String,
    pub mailbox: Option<String>,
    pub language: String,
    pub account_code: Option<String>,
    /// Ordered, validated PBX variables applied after device values.
    pub channel_variables: Vec<ChannelVariable>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleConfig {
    pub general: GeneralConfig,
    pub devices: HashMap<DeviceId, DeviceConfig>,
    pub lines: HashMap<String, LineConfig>,
    pub line_features: HashMap<String, LineFeatureConfig>,
    pub soft_key_profiles: HashMap<String, SoftKeyProfile>,
    pub(super) bindings: Vec<LineBinding>,
    pub(super) bindings_by_line: HashMap<String, Vec<usize>>,
    pub(super) bindings_by_device: HashMap<DeviceId, Vec<usize>>,
    pub(super) binding_by_button: HashMap<(DeviceId, u32), usize>,
    pub(super) device_codec_overrides: HashSet<DeviceId>,
    pub(super) line_codec_overrides: HashSet<String>,
    pub(super) device_audio_encryption_overrides: HashSet<DeviceId>,
    pub(super) line_audio_encryption_overrides: HashSet<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SoftKeyProfile {
    pub name: String,
    /// Ordered actions for every handset key mode. Missing configuration
    /// entries normalize to an empty set.
    pub sets: HashMap<KeyMode, Vec<SoftKey>>,
}

impl SoftKeyProfile {
    pub fn actions(&self, mode: KeyMode) -> &[SoftKey] {
        self.sets.get(&mode).map_or(&[], Vec::as_slice)
    }

    pub(super) fn empty(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            sets: KEY_MODES
                .into_iter()
                .map(|mode| (mode, Vec::new()))
                .collect(),
        }
    }

    pub(super) fn built_in() -> Self {
        let mut profile = Self::empty(DEFAULT_SOFT_KEY_PROFILE);
        profile.sets.extend([
            (KeyMode::OnHook, vec![SoftKey::NewCall]),
            (
                KeyMode::Connected,
                vec![SoftKey::Hold, SoftKey::EndCall, SoftKey::Transfer],
            ),
            (
                KeyMode::OnHold,
                vec![SoftKey::Resume, SoftKey::NewCall, SoftKey::EndCall],
            ),
            (KeyMode::RingIn, vec![SoftKey::Answer, SoftKey::EndCall]),
            (KeyMode::OffHook, vec![SoftKey::EndCall]),
            (
                KeyMode::ConnectedTransfer,
                vec![SoftKey::Hold, SoftKey::EndCall, SoftKey::Transfer],
            ),
            (
                KeyMode::DigitsFollowing,
                vec![SoftKey::Backspace, SoftKey::EndCall, SoftKey::Dial],
            ),
            (
                KeyMode::ConnectedConference,
                vec![SoftKey::Hold, SoftKey::EndCall],
            ),
            (KeyMode::RingOut, vec![SoftKey::EndCall]),
            (
                KeyMode::OffHookFeature,
                vec![SoftKey::Resume, SoftKey::NewCall, SoftKey::EndCall],
            ),
            (
                KeyMode::OnHookStealable,
                vec![SoftKey::Intercept, SoftKey::NewCall],
            ),
            (
                KeyMode::HoldConference,
                vec![SoftKey::Resume, SoftKey::NewCall, SoftKey::EndCall],
            ),
        ]);
        profile
    }

    pub(super) fn station_profile(&self) -> StationSoftKeyProfile {
        StationSoftKeyProfile::new(
            KEY_MODES
                .into_iter()
                .map(|mode| (mode, self.actions(mode).to_vec())),
        )
        .expect("configuration parser produced an invalid station soft-key profile")
    }
}

/// Runtime-ready timing values derived from the integer syntax accepted by
/// `sccp.conf`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GeneralTimingPolicy {
    pub keepalive: Duration,
    pub secondary_keepalive: Duration,
    pub first_digit_timeout: Duration,
    pub interdigit_timeout: Duration,
    pub call_waiting_repeat: Duration,
}

/// Runtime-ready station presentation defaults derived from general policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GeneralStationPolicy {
    pub timezone_offset_minutes: i16,
    pub date_template: DateTemplate,
    pub ring_type: RingerMode,
    pub call_waiting_tone: Option<Tone>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CallAnswerOrder {
    #[default]
    OldestFirst,
    LastFirst,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DialTerminatorConfig {
    pub character: char,
    pub record: bool,
}

/// Global dialplan contexts populated while configured lines are registered.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RegistrationConfig {
    /// Ordered, delimiter-free context names from `regcontext`.
    pub contexts: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FallbackRegistrationConfig {
    pub decision: FallbackDecision,
    pub backoff_seconds: u32,
    pub server_priority: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FallbackDecision {
    Reject,
    Accept,
    DeviceIdOdd,
    DeviceIdEven,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RealtimeTableConfig {
    pub device_family: String,
    pub line_family: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AclAction {
    Deny,
    Permit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IpNetwork {
    pub address: IpAddr,
    pub prefix: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AclRule {
    pub action: AclAction,
    pub network: IpNetwork,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AccessControlList {
    /// First-to-last ordered rules. An empty list imposes no address filter.
    pub rules: Vec<AclRule>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum NatMode {
    #[default]
    Auto,
    Off,
    AutoOff,
    On,
    AutoOn,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExternalAddress {
    Address(IpAddr),
    Hostname { name: String, refresh_seconds: u32 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdvertisedAddresses {
    pub ipv4: Option<Ipv4Addr>,
    pub ipv6: Option<Ipv6Addr>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NetworkPolicy {
    pub acl: AccessControlList,
    pub local_networks: Vec<IpNetwork>,
    pub external: Option<ExternalAddress>,
    pub advertised: AdvertisedAddresses,
    pub nat: NatMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Dscp(pub u8);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cos(pub u8);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QosClass {
    pub dscp: Dscp,
    pub cos: Cos,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QosPolicy {
    pub signaling: QosClass,
    pub audio: QosClass,
    pub video: QosClass,
}

#[derive(Clone, Eq, PartialEq)]
pub enum TlsCredentials {
    CombinedPem(PathBuf),
    SplitPem {
        certificate: PathBuf,
        private_key: PathBuf,
        trust_store: Option<PathBuf>,
    },
}

#[derive(Clone, Eq, PartialEq)]
pub struct TlsListener {
    pub bind: SocketAddr,
    pub credentials: TlsCredentials,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListenerPolicy {
    pub clear: SocketAddr,
    pub tls: Option<TlsListener>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TransportRequirement {
    Clear,
    Tls,
    #[default]
    Either,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceNetworkPolicy {
    pub acl: AccessControlList,
    pub permitted_hosts: Vec<String>,
    pub nat: NatMode,
    pub qos: QosPolicy,
    pub transport: TransportRequirement,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DndMode {
    #[default]
    Off,
    Silent,
    Reject,
}

/// State transition selected by one configured DND feature button.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DndButtonMode {
    #[default]
    Cycle,
    Silent,
    Reject,
}

impl DndButtonMode {
    pub(super) const fn canonical(self) -> Option<&'static str> {
        match self {
            Self::Cycle => None,
            Self::Silent => Some("silent"),
            Self::Reject => Some("reject"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForwardingDefaults {
    pub all_enabled: bool,
    pub busy_enabled: bool,
    pub no_answer_enabled: bool,
    pub no_answer_timeout_seconds: u32,
    pub all: Option<ForwardingDestination>,
    pub busy: Option<ForwardingDestination>,
    pub no_answer: Option<ForwardingDestination>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceFeatureDefaults {
    pub forwarding: ForwardingDefaults,
    pub dnd_enabled: bool,
    pub dnd: DndMode,
    pub privacy_enabled: bool,
    pub privacy: bool,
    /// Every configured feature-button instance, including defaults that are
    /// explicitly or implicitly false.
    pub buttons: HashMap<u32, bool>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VoicemailDefaults {
    pub number: Option<VoicemailDestination>,
    pub transfer_destination: Option<VoicemailDestination>,
}

impl VoicemailDefaults {
    pub fn divert_destination(&self) -> Option<&VoicemailDestination> {
        self.transfer_destination.as_ref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PickupConfig {
    pub call_groups: BTreeSet<u8>,
    pub pickup_groups: BTreeSet<u8>,
    pub named_call_groups: BTreeSet<String>,
    pub named_pickup_groups: BTreeSet<String>,
    pub directed: bool,
    /// `None` means use the line's normal dialplan context.
    pub directed_context: Option<String>,
    pub answer_directed: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ParkingRetrievalBehavior {
    /// Immediately retrieve the call when the lot contains exactly one call;
    /// otherwise show the parked-call menu.
    #[default]
    RetrieveSingle,
    /// Show the parked-call menu even when the lot contains one call.
    AlwaysShowMenu,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParkingLotButtonConfig {
    pub lot: String,
    pub retrieval: ParkingRetrievalBehavior,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceParkingConfig {
    pub enabled: bool,
    /// Typed settings keyed by feature-button instance.
    pub feature_buttons: HashMap<u32, ParkingLotButtonConfig>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LineParkingConfig {
    /// Named Asterisk parking lot selected when this line parks a call.
    pub lot: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConferenceDialingConfig {
    pub enabled: bool,
    /// Opaque application option string. Its interpretation belongs to the
    /// selected Asterisk conference application.
    pub application_options: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceConferenceConfig {
    pub allowed: bool,
    /// `None` explicitly disables conference music on hold.
    pub music_on_hold_class: Option<String>,
    pub play_general_announcements: bool,
    pub play_participant_announcements: bool,
    pub mute_on_entry: bool,
    pub show_conference_list: bool,
    pub dialing: ConferenceDialingConfig,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LineConferenceConfig {
    /// `None` inherits the device conference-dialing default.
    pub enabled: Option<bool>,
    pub destination: Option<String>,
    /// `None` inherits device options. `Some("")` explicitly supplies no
    /// application options.
    pub application_options: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedConferenceDialing {
    pub enabled: bool,
    pub destination: Option<String>,
    pub application_options: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutoAnswerConfig {
    pub ring_time_seconds: u32,
    pub tone: Tone,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuestHotlineConfig {
    /// Whether an otherwise unknown device may register on the shared guest
    /// hotline line.
    pub enabled: bool,
    pub extension: Option<HotlineDestination>,
    pub context: String,
    pub label: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LineHotlineConfig {
    /// Destination dialed when this configured line goes off-hook without an
    /// explicitly selected line.
    pub destination: Option<HotlineDestination>,
}

/// Tones used while an outbound call is collecting digits on a logical line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineDialToneConfig {
    pub initial: Tone,
    /// An empty configured value disables the secondary dial tone.
    pub secondary_prefix: Option<String>,
    pub secondary: Tone,
}

/// A line's optional PIN for handset Extension Mobility login.
///
/// The value deliberately has a redacted `Debug` representation so a complete
/// normalized configuration can be logged without exposing credentials.
#[derive(Clone, Eq, PartialEq)]
pub struct MobilityPin(pub(super) String);

impl MobilityPin {
    /// Verify a candidate without returning early for a mismatched byte or
    /// length. Both inputs are bounded by [`MAX_MOBILITY_PIN_DIGITS`], so every
    /// verification performs exactly that many byte comparisons.
    pub fn verify(&self, candidate: &str) -> bool {
        let expected = self.0.as_bytes();
        let actual = candidate.as_bytes();
        let mut difference = expected.len() ^ actual.len();
        for index in 0..MAX_MOBILITY_PIN_DIGITS {
            let expected = expected.get(index).copied().unwrap_or_default();
            let actual = actual.get(index).copied().unwrap_or_default();
            difference |= usize::from(expected ^ actual);
        }
        difference == 0
    }

    pub fn digits(&self) -> usize {
        self.0.len()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LineMobilityConfig {
    pub pin: Option<MobilityPin>,
}

/// One configured registration extension before global-context expansion.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RegistrationExtension {
    pub extension: String,
    /// An explicit context overrides the global context list for this entry.
    pub context: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LineRegistrationConfig {
    /// Ordered, delimiter-free entries from `regexten`. An omitted or empty
    /// value normalizes to the logical line number.
    pub extensions: Vec<RegistrationExtension>,
}

/// A fully resolved extension/context pair ready for a dialplan adapter.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RegistrationTarget {
    pub extension: String,
    pub context: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum VideoMode {
    Off,
    User,
    #[default]
    Auto,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum JitterBufferImplementation {
    #[default]
    Fixed,
    Adaptive,
}

/// Global Asterisk receive-side jitter-buffer policy. These settings are not
/// valid in device or line sections.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JitterBufferConfig {
    pub enabled: bool,
    pub forced: bool,
    pub log_frames: bool,
    pub max_size_ms: u32,
    pub resync_threshold_ms: u32,
    pub implementation: JitterBufferImplementation,
}

impl JitterBufferConfig {
    pub const fn should_configure_channel(self, direct_media: bool) -> bool {
        self.enabled && (self.forced || !direct_media)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceMediaConfig {
    pub codecs: Vec<Codec>,
    pub audio_encryption: MediaEncryptionPolicy,
    pub dtmf_mode: DtmfMode,
    pub direct_media: bool,
    pub early_media: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineMediaConfig {
    pub codecs: Vec<Codec>,
    pub audio_encryption: MediaEncryptionPolicy,
    pub video_mode: VideoMode,
    pub audio_processing: AudioProcessingPolicy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedMediaConfig {
    pub codecs: Vec<Codec>,
    pub audio_encryption: MediaEncryptionPolicy,
    pub dtmf_mode: DtmfMode,
    pub direct_media: bool,
    pub early_media: bool,
    pub video_mode: VideoMode,
    pub audio_processing: AudioProcessingPolicy,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RedialMode {
    #[default]
    LastNumber,
    PlacedCallsMenu,
}

/// Per-device call-history and hinted-line presentation policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceCallUiConfig {
    pub redial_mode: RedialMode,
    pub hinted_ringing_notification: bool,
    pub mwi_lamp_mode: LampMode,
    pub mwi_on_call: bool,
    pub legacy_code_page: LegacyCodePage,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineFeatureConfig {
    pub incoming_limit: u32,
    pub voicemail: VoicemailDefaults,
    pub pickup: PickupConfig,
    pub parking: LineParkingConfig,
    pub conference: LineConferenceConfig,
    pub hotline: LineHotlineConfig,
    pub dial_tones: LineDialToneConfig,
    pub mobility: LineMobilityConfig,
    pub registration: LineRegistrationConfig,
    pub media: LineMediaConfig,
}

/// An Asterisk dialplan hint addressed independently from the number dialed by
/// its SCCP BLF button.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct HintTarget {
    pub(super) extension: String,
    pub(super) context: String,
}

impl HintTarget {
    pub fn parse(value: &str) -> Result<Self, ConfigError> {
        let value = value.trim();
        let Some((extension, context)) = value.split_once('@') else {
            return Err(ConfigError::InvalidValue {
                key: "button.blf.hint".into(),
                value: value.into(),
            });
        };
        let extension = extension.trim();
        let context = context.trim();
        if extension.is_empty() || context.is_empty() || context.contains('@') {
            return Err(ConfigError::InvalidValue {
                key: "button.blf.hint".into(),
                value: value.into(),
            });
        }
        Ok(Self {
            extension: extension.into(),
            context: context.into(),
        })
    }

    pub fn extension(&self) -> &str {
        &self.extension
    }

    pub fn context(&self) -> &str {
        &self.context
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineBinding {
    pub device_id: DeviceId,
    pub line_instance: u32,
    pub appearance: LineAppearance,
    pub line: LineConfig,
}
