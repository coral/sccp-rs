//! Application-facing identities, station configuration, and call/media data.
//!
//! These types sit above the raw message codec. Applications normally build a
//! [`DeviceDefinition`], validate it, and pass it to
//! [`crate::server::Server::bind`]. Runtime events then refer to calls, lines,
//! conferences, and application transactions through the strongly typed IDs
//! in this module instead of interchangeable integers.

use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::num::NonZeroU64;
use std::ops::Deref;
use std::str::FromStr;

use crate::message::values::{
    CallType, Codec, DeviceType, EchoCancellation, KeyMode, ProtocolVersion, SilenceSuppression,
    SoftKey, Tone,
};
use crate::message::wire::CodecError;

pub(crate) const MAX_STATION_BUTTON_INSTANCE: u32 = u8::MAX as u32;

/// Validated date and time display template advertised during registration.
///
/// Examples include `D/M/Y`, `Y.M.D`, and `M-D-YYA`; the final `A` selects a
/// twelve-hour clock.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DateTemplate(String);

impl DateTemplate {
    /// Validates and constructs a station date/time display template.
    ///
    /// The template must contain `D`, `M`, and either `Y` or `YY` exactly once,
    /// separated by `/`, `.`, `-`, or a space. A trailing `A` requests a
    /// twelve-hour clock.
    pub fn new(value: impl Into<String>) -> Result<Self, CodecError> {
        let value = value.into();
        let date = value.strip_suffix('A').unwrap_or(&value);
        let mut fields = date.split(|character: char| !character.is_ascii_alphabetic());
        let parsed = [fields.next(), fields.next(), fields.next()];
        let separators = date
            .bytes()
            .filter(|byte| !byte.is_ascii_alphabetic())
            .collect::<Vec<_>>();
        let valid_fields = matches!(parsed, [Some(_), Some(_), Some(_)])
            && fields.next().is_none()
            && parsed
                .into_iter()
                .flatten()
                .all(|field| matches!(field, "D" | "M" | "Y" | "YY"))
            && parsed
                .into_iter()
                .flatten()
                .filter(|field| *field == "D")
                .count()
                == 1
            && parsed
                .into_iter()
                .flatten()
                .filter(|field| *field == "M")
                .count()
                == 1
            && parsed
                .into_iter()
                .flatten()
                .filter(|field| matches!(*field, "Y" | "YY"))
                .count()
                == 1;
        if value.len() > 7
            || separators.len() != 2
            || !separators
                .iter()
                .all(|byte| matches!(byte, b'/' | b'.' | b'-' | b' '))
            || !valid_fields
        {
            return Err(CodecError::InvalidDefinition(
                "date template must contain D, M, and Y/YY once, two supported separators, and an optional trailing A for 12-hour time"
                    .into(),
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn uses_twelve_hour_clock(&self) -> bool {
        self.0.ends_with('A')
    }
}

impl AsRef<str> for DateTemplate {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for DateTemplate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for DateTemplate {
    type Err = CodecError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl TryFrom<String> for DateTemplate {
    type Error = CodecError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl Default for DateTemplate {
    fn default() -> Self {
        Self("D/M/Y".into())
    }
}

impl fmt::Debug for DateTemplate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("DateTemplate")
            .field(&self.0)
            .finish()
    }
}

/// Exact SCCP station name, normally `SEP` followed by twelve MAC digits.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DeviceId(String);

impl DeviceId {
    /// Validates and canonicalizes a station identifier.
    ///
    /// Leading and trailing whitespace is removed, ASCII letters are folded
    /// to uppercase, and the result must contain at most 15 alphanumeric
    /// characters.
    pub fn new(value: impl Into<String>) -> Result<Self, CodecError> {
        let value = value.into().trim().to_ascii_uppercase();
        if value.is_empty() || value.len() > 15 || !value.bytes().all(|b| b.is_ascii_alphanumeric())
        {
            return Err(CodecError::InvalidDeviceId(value));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for DeviceId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for DeviceId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

/// Monotonically allocated identity for one accepted station session.
///
/// Events from a replaced connection retain its earlier generation, allowing
/// consumers to discard work that arrives after a station reconnects.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SessionGeneration(NonZeroU64);

impl SessionGeneration {
    /// Rejects the reserved zero value rather than creating an ambiguous
    /// session identity.
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

impl From<SessionGeneration> for u64 {
    fn from(value: SessionGeneration) -> Self {
        value.get()
    }
}

impl fmt::Display for SessionGeneration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for DeviceId {
    type Err = CodecError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl TryFrom<String> for DeviceId {
    type Error = CodecError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

macro_rules! id_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(pub u32);

        impl $name {
            pub const fn new(value: u32) -> Self {
                Self(value)
            }

            pub const fn get(self) -> u32 {
                self.0
            }
        }

        impl From<u32> for $name {
            fn from(value: u32) -> Self {
                Self(value)
            }
        }

        impl From<$name> for u32 {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id_newtype!(/// A line or button instance on a station.
    LineInstance);
id_newtype!(/// A device-visible call reference.
    CallReference);
id_newtype!(/// A media passthrough-party identifier.
    PassthroughPartyId);
id_newtype!(/// A stable identifier for one device's appearance of a logical line.
    AppearanceId);
id_newtype!(/// A conference identifier.
    ConferenceId);
id_newtype!(/// A stable identifier for a participant in a conference.
    ParticipantId);
id_newtype!(/// An SCCP application identifier.
    ApplicationId);
id_newtype!(/// An application transaction identifier.
    TransactionId);

/// ECN-zeroed traffic-class octet carried by station media commands.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MediaTrafficClass(u8);

impl MediaTrafficClass {
    pub const fn from_wire(value: u8) -> Self {
        Self(value)
    }

    pub const fn from_dscp(dscp: u8) -> Option<Self> {
        if dscp <= 63 {
            Some(Self(dscp << 2))
        } else {
            None
        }
    }

    pub const fn get(self) -> u8 {
        self.0
    }
}

impl From<MediaTrafficClass> for u8 {
    fn from(value: MediaTrafficClass) -> Self {
        value.get()
    }
}

impl From<MediaTrafficClass> for u32 {
    fn from(value: MediaTrafficClass) -> Self {
        u32::from(value.get())
    }
}

/// Application-local call identifier.
///
/// This is deliberately wider than the station-visible [`CallReference`]. The
/// server maps between the two so a long-running application does not need to
/// reuse its own call identities when the wire namespace rolls over.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CallId(pub u64);

impl CallId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for CallId {
    fn from(value: u64) -> Self {
        Self::new(value)
    }
}

impl From<CallId> for u64 {
    fn from(value: CallId) -> Self {
        value.get()
    }
}

impl fmt::Display for CallId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// A logical directory number and its default station label.
///
/// A logical line may be presented on more than one station through distinct
/// [`LineAppearance`] values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineDefinition {
    /// Nonempty directory number; [`DeviceDefinition::validate`] limits it to
    /// 24 bytes when used by a station.
    pub number: String,
    pub display_name: String,
}

/// Optional caller identity substitutions for one line appearance.
///
/// `None` keeps the identity supplied by the call owner; an empty string is an
/// explicit empty override.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CallerIdOverride {
    pub name: Option<String>,
    pub number: Option<String>,
}

/// Incoming-ring policy for a line appearance.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AppearanceRingMode {
    #[default]
    Normal,
    Silent,
    Disabled,
}

/// One device's configured presentation of a logical line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineAppearance {
    /// Stable within the owning device definition.
    pub id: AppearanceId,
    /// Station-visible line instance; instances normally begin at one.
    pub instance: u32,
    pub line: LineDefinition,
    /// Optional button label overriding [`LineDefinition::display_name`].
    pub label: Option<String>,
    pub caller_id: CallerIdOverride,
    pub ring_mode: AppearanceRingMode,
    /// Tone used when a new outgoing call begins on this appearance.
    pub initial_tone: Tone,
    /// Optional subscription identity used by presence integrations.
    pub subscription_identity: Option<String>,
    pub privacy: bool,
}

impl LineAppearance {
    /// Creates an appearance using the instance as its stable appearance ID.
    pub fn new(instance: u32, line: LineDefinition) -> Self {
        Self {
            id: AppearanceId::new(instance),
            instance,
            line,
            label: None,
            caller_id: CallerIdOverride::default(),
            ring_mode: AppearanceRingMode::Normal,
            initial_tone: Tone::InsideDial,
            subscription_identity: None,
            privacy: false,
        }
    }

    /// Returns the explicit button label or the logical line's display name.
    pub fn display_label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.line.display_name)
    }
}

impl Deref for LineAppearance {
    type Target = LineDefinition;

    fn deref(&self) -> &Self::Target {
        &self.line
    }
}

/// A programmable button that immediately dials a configured destination.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpeedDialDefinition {
    /// Nonzero station button instance, unique among speed-dial buttons.
    pub instance: u32,
    pub number: String,
    pub display_name: String,
}

/// A speed-dial button whose lamp and icon follow an external presence target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlfSpeedDialDefinition {
    /// Nonzero station feature instance, shared with other feature buttons.
    pub instance: u32,
    pub number: String,
    pub display_name: String,
}

/// Semantic state of a monitored speed-dial target.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BlfState {
    Idle,
    Ringing,
    Busy,
    Held,
    DoNotDisturb,
    Unavailable,
    #[default]
    Unknown,
}

/// Caller information that policy has permitted a monitored station to see.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BlfCallerInfo {
    pub name: String,
    pub number: String,
}

impl BlfCallerInfo {
    /// Formats the permitted name and number for station presentation.
    pub fn display(&self) -> String {
        match (self.name.trim(), self.number.trim()) {
            ("", "") => String::new(),
            ("", number) => number.to_owned(),
            (name, "") => name.to_owned(),
            (name, number) => format!("{name} ({number})"),
        }
    }
}

/// A programmable feature button and its station-visible label.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeatureDefinition {
    /// Nonzero station button instance, unique among feature buttons.
    pub instance: u32,
    pub label: String,
    pub feature: crate::message::values::ButtonType,
}

pub(crate) const MAX_STATION_FEATURE_LABEL_BYTES: usize = 39;

/// A programmable feature button that controls handset-scoped call recording.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordingButtonDefinition {
    pub instance: u32,
    /// Station-visible label used while recording is inactive.
    pub label: String,
}

/// A programmable button that opens a phone-hosted HTTP service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceDefinition {
    /// Nonzero station button instance, unique among service buttons.
    pub instance: u32,
    pub label: String,
    /// Absolute HTTP(S) URL opened by the station.
    ///
    /// Validation rejects fragments, control characters, excessive query
    /// parameters, and values that exceed the station wire limits.
    pub url: String,
}

/// An expansion module and the station slot where it is attached.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AddonModuleDefinition {
    /// One-based expansion-module slot.
    pub slot: u32,
    pub device_type: crate::message::values::DeviceType,
}

impl AddonModuleDefinition {
    /// Number of physical programmable keys supplied by this sidecar model.
    pub const fn button_capacity(&self) -> Option<usize> {
        use crate::message::values::DeviceType;

        match self.device_type {
            DeviceType::CiscoAddon7914 => Some(14),
            DeviceType::CiscoAddon7915_12 | DeviceType::CiscoAddon7916_12 => Some(12),
            DeviceType::CiscoAddon7915_24 | DeviceType::CiscoAddon7916_24 => Some(24),
            DeviceType::AddonSpa500s | DeviceType::AddonSpa500ds | DeviceType::AddonSpa932ds => {
                Some(32)
            }
            _ => None,
        }
    }
}

/// One entry in a station's ordered physical button layout.
///
/// [`DeviceDefinition::validate`] checks instance uniqueness, expansion-module
/// capacity, and the overall wire limit before the layout is sent to a phone.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ButtonDefinition {
    Line(LineAppearance),
    SpeedDial(SpeedDialDefinition),
    BlfSpeedDial(BlfSpeedDialDefinition),
    Feature(FeatureDefinition),
    Recording(RecordingButtonDefinition),
    Service(ServiceDefinition),
    AddonModule(AddonModuleDefinition),
    Unused,
}

/// Ordered soft-key actions advertised for every known station key mode.
///
/// The protocol template remains the canonical 32-entry action catalog; these
/// sets choose which catalog entries appear in each mode and in which order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SoftKeyProfile {
    sets: HashMap<KeyMode, Vec<SoftKey>>,
}

impl SoftKeyProfile {
    /// Maximum number of actions that one station key mode can advertise.
    pub const MAX_KEYS_PER_MODE: usize = 16;

    /// Builds and validates a complete profile.
    ///
    /// Every known [`KeyMode`] must occur exactly once. Use [`Self::empty`] as
    /// a convenient base when only a few modes should expose actions.
    pub fn new(
        sets: impl IntoIterator<Item = (KeyMode, Vec<SoftKey>)>,
    ) -> Result<Self, CodecError> {
        let profile = Self {
            sets: sets.into_iter().collect(),
        };
        profile.validate()?;
        Ok(profile)
    }

    pub fn empty() -> Self {
        Self {
            sets: KeyMode::ALL_KNOWN
                .iter()
                .copied()
                .map(|mode| (mode, Vec::new()))
                .collect(),
        }
    }

    /// The wire-compatible profile used when a station has no configured
    /// override.
    pub fn built_in() -> Self {
        let mut profile = Self::empty();
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

    pub fn actions(&self, mode: KeyMode) -> &[SoftKey] {
        self.sets.get(&mode).map_or(&[], Vec::as_slice)
    }

    pub fn allows(&self, mode: KeyMode, action: SoftKey) -> bool {
        action.is_known() && self.actions(mode).contains(&action)
    }

    /// Returns the station bit mask enabling every configured action in `mode`.
    pub fn valid_mask(&self, mode: KeyMode) -> u32 {
        let count = self.actions(mode).len();
        if count == 0 { 0 } else { (1_u32 << count) - 1 }
    }

    /// Actions whose labels are needed in the station template. The built-in
    /// profile intentionally retains the historical complete catalog bytes.
    pub fn template_actions(&self) -> Vec<SoftKey> {
        if self == &Self::built_in() {
            return SoftKey::ALL_KNOWN.to_vec();
        }
        let configured: HashSet<_> = KeyMode::ALL_KNOWN
            .iter()
            .flat_map(|mode| self.actions(*mode).iter().copied())
            .collect();
        SoftKey::ALL_KNOWN
            .iter()
            .copied()
            .filter(|action| configured.contains(action))
            .collect()
    }

    /// Verifies completeness, per-mode limits, known values, and uniqueness.
    pub fn validate(&self) -> Result<(), CodecError> {
        if self.sets.len() != KeyMode::ALL_KNOWN.len()
            || KeyMode::ALL_KNOWN
                .iter()
                .any(|mode| !self.sets.contains_key(mode))
        {
            return Err(CodecError::InvalidDefinition(
                "soft-key profile must define every known key mode".into(),
            ));
        }
        for (&mode, actions) in &self.sets {
            if !mode.is_known() {
                return Err(CodecError::InvalidDefinition(format!(
                    "soft-key profile contains unknown key mode {}",
                    mode.wire_value()
                )));
            }
            if actions.len() > Self::MAX_KEYS_PER_MODE {
                return Err(CodecError::InvalidDefinition(format!(
                    "soft-key mode {} contains {} actions; the protocol limit is {}",
                    mode.wire_value(),
                    actions.len(),
                    Self::MAX_KEYS_PER_MODE
                )));
            }
            let mut seen = HashSet::new();
            for &action in actions {
                if !action.is_known() {
                    return Err(CodecError::InvalidDefinition(format!(
                        "soft-key mode {} contains unknown action {}",
                        mode.wire_value(),
                        action.wire_value()
                    )));
                }
                if !seen.insert(action) {
                    return Err(CodecError::InvalidDefinition(format!(
                        "soft-key mode {} repeats action {}",
                        mode.wire_value(),
                        action.wire_value()
                    )));
                }
            }
        }
        Ok(())
    }
}

impl Default for SoftKeyProfile {
    fn default() -> Self {
        Self::built_in()
    }
}

const MAX_STATION_HEADER_BYTES: usize = 39;

/// Complete phone-facing configuration for one station.
///
/// Call [`Self::validate`] before starting or reconfiguring a server. A valid
/// definition has at least one line, unique nonzero button instances, a
/// complete soft-key profile, and no more physical buttons than the protocol
/// can advertise.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceDefinition {
    pub id: DeviceId,
    /// Station identity shown in the primary line's idle-screen header.
    ///
    /// An empty value falls back to the primary line's directory number.
    /// Nonempty values may contain at most 39 bytes and no control characters.
    pub description: String,
    pub transport: StationTransportRequirement,
    /// Socket marking selected after this station identifies itself. `None`
    /// inherits the server-wide signaling policy.
    pub signaling_qos: Option<SignalingQos>,
    /// Physical station buttons in display order.
    ///
    /// Line instances remain protocol-level identifiers carried by
    /// [`LineAppearance`]; they are not inferred from the vector index once
    /// non-line buttons are present.
    pub buttons: Vec<ButtonDefinition>,
    /// Fully resolved station soft-key policy.
    pub soft_keys: SoftKeyProfile,
    /// Per-station presentation behavior that belongs at the phone boundary.
    pub ui: StationUiPolicy,
}

/// Transport admission policy configured for one station.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum StationTransportRequirement {
    Clear,
    Secure,
    #[default]
    Either,
}

/// Transport used by an accepted station session.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum StationTransport {
    Clear,
    Secure,
}

/// Network-layer marking for station signaling traffic.
///
/// DSCP occupies the upper six bits of the IPv4 type-of-service or IPv6
/// traffic-class field. COS is applied as a socket priority on platforms that
/// expose that facility; unsupported priority marking is reported separately
/// from DSCP and does not make a session unusable.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct SignalingQos {
    pub dscp: u8,
    pub cos: u8,
}

impl SignalingQos {
    pub const fn new(dscp: u8, cos: u8) -> Self {
        Self { dscp, cos }
    }

    pub(crate) fn validate(self) -> Result<(), CodecError> {
        if self.dscp > 63 {
            return Err(CodecError::InvalidDefinition(format!(
                "signaling DSCP {} is outside 0..=63",
                self.dscp
            )));
        }
        if self.cos > 7 {
            return Err(CodecError::InvalidDefinition(format!(
                "signaling COS {} is outside 0..=7",
                self.cos
            )));
        }
        Ok(())
    }
}

/// Phone-facing behavior that can vary independently for each configured
/// station without changing logical line ownership.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StationUiPolicy {
    /// Ask capable displays to open their native placed-calls application when
    /// Redial is pressed. Older displays retain last-number redial.
    pub placed_calls_redial_menu: bool,
    /// Permit a ringing-only notification in addition to the ordinary BLF
    /// icon/lamp projection. Non-ringing BLF states are always delivered.
    pub hinted_ringing_notification: bool,
    /// Keep a speed-dial destination in digit collection until the normal
    /// interdigit timeout or an explicit dial terminator commits it.
    pub speed_dial_await_further_digits: bool,
    /// Lamp cadence used while the station has waiting voicemail.
    pub mwi_lamp_mode: crate::message::values::LampMode,
    /// Keep MWI visible while any call is active on the station.
    pub mwi_on_call: bool,
    /// Single-byte encoding used only when the handset does not advertise
    /// native UTF-8 text support.
    pub legacy_code_page: LegacyCodePage,
}

/// Single-byte character set used for stations without native UTF-8 support.
/// Characters outside the selected set are replaced during encoding.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LegacyCodePage {
    #[default]
    Iso8859_1,
    Ascii,
}

impl Default for StationUiPolicy {
    fn default() -> Self {
        Self {
            placed_calls_redial_menu: false,
            hinted_ringing_notification: false,
            speed_dial_await_further_digits: false,
            mwi_lamp_mode: crate::message::values::LampMode::On,
            mwi_on_call: false,
            legacy_code_page: LegacyCodePage::Iso8859_1,
        }
    }
}

impl DeviceDefinition {
    /// Validates the station definition and its nested button/service policies.
    pub fn validate(&self) -> Result<(), CodecError> {
        // One canonical ButtonTemplate frame carries 42 definitions. Larger
        // station/sidecar layouts are sent as offset chunks; retain the
        // library's validated logical-layout ceiling independently.
        const MAX_BUTTONS: usize = 256;

        if self.description.len() > MAX_STATION_HEADER_BYTES
            || self.description.chars().any(char::is_control)
        {
            return Err(CodecError::InvalidDefinition(format!(
                "device {} has an invalid station-header description",
                self.id
            )));
        }

        self.soft_keys.validate()?;
        if let Some(signaling_qos) = self.signaling_qos {
            signaling_qos.validate()?;
        }

        if self.buttons.len() > MAX_BUTTONS {
            return Err(CodecError::InvalidDefinition(format!(
                "device {} has {} buttons; the logical layout limit is {MAX_BUTTONS}",
                self.id,
                self.buttons.len()
            )));
        }

        let mut expanded_buttons = 0_usize;
        let mut addon_buttons_remaining = None;
        for button in &self.buttons {
            if let ButtonDefinition::AddonModule(addon) = button {
                expanded_buttons += addon_buttons_remaining.take().unwrap_or_default();
                addon_buttons_remaining = Some(addon.button_capacity().ok_or_else(|| {
                    CodecError::InvalidDefinition(format!(
                        "device {} has unsupported addon-module type {}",
                        self.id,
                        addon.device_type.wire_value()
                    ))
                })?);
                continue;
            }
            expanded_buttons += 1;
            if let Some(remaining) = &mut addon_buttons_remaining {
                if *remaining == 0 {
                    return Err(CodecError::InvalidDefinition(format!(
                        "device {} configures more buttons than its addon module provides",
                        self.id
                    )));
                }
                *remaining -= 1;
            }
        }
        expanded_buttons += addon_buttons_remaining.unwrap_or_default();
        if expanded_buttons > MAX_BUTTONS {
            return Err(CodecError::InvalidDefinition(format!(
                "device {} expands to {expanded_buttons} buttons; the logical layout limit is {MAX_BUTTONS}",
                self.id
            )));
        }

        let mut instances = HashSet::new();
        let mut appearance_ids = HashSet::new();
        for button in &self.buttons {
            let Some((kind, instance)) = button.instance_key() else {
                continue;
            };
            if instance == 0 {
                return Err(CodecError::InvalidDefinition(format!(
                    "device {} has a {kind} button with instance zero",
                    self.id
                )));
            }
            // Every ordinary button instance is encoded into the one-byte
            // ButtonTemplate definition field. Add-on slots are logical
            // expansion-module positions and retain their separate u32
            // contract; they are not emitted as template definitions.
            if kind != ButtonNamespace::AddonModule && instance > MAX_STATION_BUTTON_INSTANCE {
                return Err(CodecError::InvalidDefinition(format!(
                    "device {} has a {kind} button with instance {instance}; maximum wire instance is {}",
                    self.id, MAX_STATION_BUTTON_INSTANCE
                )));
            }
            if !instances.insert((kind, instance)) {
                return Err(CodecError::InvalidDefinition(format!(
                    "device {} repeats {kind} button instance {instance}",
                    self.id
                )));
            }
            match button {
                ButtonDefinition::Line(appearance) => {
                    if appearance.id.get() == 0 {
                        return Err(CodecError::InvalidDefinition(format!(
                            "device {} has a line appearance with identifier zero",
                            self.id
                        )));
                    }
                    if !appearance_ids.insert(appearance.id) {
                        return Err(CodecError::InvalidDefinition(format!(
                            "device {} repeats line appearance identifier {}",
                            self.id, appearance.id
                        )));
                    }
                }
                ButtonDefinition::Recording(recording) => {
                    validate_recording_button_definition(&self.id, recording)?;
                }
                ButtonDefinition::Service(service) => {
                    validate_service_definition(&self.id, service)?;
                }
                _ => {}
            }
        }

        let lines: Vec<_> = self.lines().collect();
        if lines.is_empty() {
            return Err(CodecError::InvalidDefinition(format!(
                "device {} has no lines",
                self.id
            )));
        }
        // Extension Mobility appearances have independent slot lifetimes. If
        // an earlier slot logs out while a later one remains, the live button
        // template is intentionally sparse until that slot is reused.
        let permits_sparse_lines = self.buttons.iter().any(|button| {
            matches!(
                button,
                ButtonDefinition::Feature(feature)
                    if feature.feature == crate::message::values::ButtonType::Mobility
            )
        });
        for (expected, line) in (1_u32..).zip(lines) {
            if !permits_sparse_lines && line.instance != expected {
                return Err(CodecError::InvalidDefinition(format!(
                    "device {} line instances must be contiguous from 1",
                    self.id
                )));
            }
            if line.number.is_empty() || line.number.len() > 24 {
                return Err(CodecError::InvalidDefinition(format!(
                    "device {} has an invalid line number",
                    self.id
                )));
            }
        }
        Ok(())
    }

    pub fn lines(&self) -> impl Iterator<Item = &LineAppearance> {
        self.buttons.iter().filter_map(|button| match button {
            ButtonDefinition::Line(line) => Some(line),
            _ => None,
        })
    }

    pub fn line(&self, instance: u32) -> Option<&LineAppearance> {
        self.lines().find(|line| line.instance == instance)
    }

    pub fn first_line(&self) -> Option<&LineAppearance> {
        self.lines().next()
    }

    pub fn line_count(&self) -> usize {
        self.lines().count()
    }

    pub(crate) fn feature_button(&self, instance: u32) -> Option<&FeatureDefinition> {
        self.buttons.iter().find_map(|button| match button {
            ButtonDefinition::Feature(feature) if feature.instance == instance => Some(feature),
            _ => None,
        })
    }

    pub(crate) fn recording_button(&self, instance: u32) -> Option<&RecordingButtonDefinition> {
        self.buttons.iter().find_map(|button| match button {
            ButtonDefinition::Recording(recording) if recording.instance == instance => {
                Some(recording)
            }
            _ => None,
        })
    }

    pub(crate) fn blf_button(&self, instance: u32) -> Option<&BlfSpeedDialDefinition> {
        self.buttons.iter().find_map(|button| match button {
            ButtonDefinition::BlfSpeedDial(blf) if blf.instance == instance => Some(blf),
            _ => None,
        })
    }
}

fn validate_recording_button_definition(
    device: &DeviceId,
    recording: &RecordingButtonDefinition,
) -> Result<(), CodecError> {
    if recording.label.is_empty()
        || recording.label.len() > MAX_STATION_FEATURE_LABEL_BYTES
        || recording.label.chars().any(char::is_control)
    {
        return Err(CodecError::InvalidDefinition(format!(
            "device {device} has an invalid recording-button label"
        )));
    }
    Ok(())
}

fn validate_service_definition(
    device: &DeviceId,
    service: &ServiceDefinition,
) -> Result<(), CodecError> {
    const MAX_SERVICE_URL_BYTES: usize = 255;
    const MAX_SERVICE_PARAMETERS: usize = 32;
    const MAX_SERVICE_PARAMETER_BYTES: usize = 128;

    if service.label.is_empty()
        || service.label.len() > MAX_STATION_FEATURE_LABEL_BYTES
        || service.label.chars().any(char::is_control)
    {
        return Err(CodecError::InvalidDefinition(format!(
            "device {device} has an invalid service label"
        )));
    }
    if service.url.is_empty()
        || service.url.len() > MAX_SERVICE_URL_BYTES
        || service.url.chars().any(char::is_control)
    {
        return Err(CodecError::InvalidDefinition(format!(
            "device {device} has an invalid service URL"
        )));
    }
    let url = url::Url::parse(&service.url).map_err(|_| {
        CodecError::InvalidDefinition(format!("device {device} has a malformed service URL"))
    })?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || url.fragment().is_some()
    {
        return Err(CodecError::InvalidDefinition(format!(
            "device {device} service URL must be HTTP(S) without a fragment"
        )));
    }
    let parameters = url.query_pairs().collect::<Vec<_>>();
    if parameters.len() > MAX_SERVICE_PARAMETERS
        || parameters.iter().any(|(name, value)| {
            name.is_empty()
                || name.len() > MAX_SERVICE_PARAMETER_BYTES
                || value.len() > MAX_SERVICE_PARAMETER_BYTES
                || name.chars().chain(value.chars()).any(char::is_control)
        })
    {
        return Err(CodecError::InvalidDefinition(format!(
            "device {device} service URL has invalid or excessive query parameters"
        )));
    }
    Ok(())
}

impl ButtonDefinition {
    fn instance_key(&self) -> Option<(ButtonNamespace, u32)> {
        match self {
            Self::Line(definition) => Some((ButtonNamespace::Line, definition.instance)),
            Self::SpeedDial(definition) => Some((ButtonNamespace::SpeedDial, definition.instance)),
            Self::BlfSpeedDial(definition) => Some((ButtonNamespace::Feature, definition.instance)),
            Self::Feature(definition) => Some((ButtonNamespace::Feature, definition.instance)),
            Self::Recording(definition) => Some((ButtonNamespace::Feature, definition.instance)),
            Self::Service(definition) => Some((ButtonNamespace::Service, definition.instance)),
            Self::AddonModule(definition) => Some((ButtonNamespace::AddonModule, definition.slot)),
            Self::Unused => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum ButtonNamespace {
    Line,
    SpeedDial,
    Feature,
    Service,
    AddonModule,
}

impl fmt::Display for ButtonNamespace {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Line => "line",
            Self::SpeedDial => "speed dial",
            Self::Feature => "feature",
            Self::Service => "service URL",
            Self::AddonModule => "addon module",
        })
    }
}

/// Negotiated identity and network metadata for a live station session.
///
/// This value is emitted with registration events after the server has
/// validated the device definition, transport policy, and protocol version.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceRegistration {
    pub id: DeviceId,
    pub peer: SocketAddr,
    pub transport: StationTransport,
    pub reported_address: Option<Ipv4Addr>,
    pub reported_ipv6_address: Option<Ipv6Addr>,
    pub device_type: DeviceType,
    pub protocol: ProtocolVersion,
    pub firmware: String,
}

impl DeviceRegistration {
    /// Return the station-reported address matching the signaling peer's
    /// effective address family. IPv4-mapped IPv6 peers use the IPv4 report.
    pub fn reported_address_for_peer(&self) -> Option<IpAddr> {
        match self.peer.ip() {
            IpAddr::V4(_) => self.reported_address.map(IpAddr::V4),
            IpAddr::V6(peer) => peer.to_ipv4_mapped().map_or_else(
                || self.reported_ipv6_address.map(IpAddr::V6),
                |_| self.reported_address.map(IpAddr::V4),
            ),
        }
    }
}

/// Direction of a call relative to the station.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallDirection {
    Inbound,
    Outbound,
}

impl From<CallDirection> for CallType {
    fn from(value: CallDirection) -> Self {
        match value {
            CallDirection::Inbound => Self::Inbound,
            CallDirection::Outbound => Self::Outbound,
        }
    }
}

/// Party identity and redirection history presented for a call.
///
/// Empty strings mean the corresponding identity is unavailable. Presentation
/// restrictions are carried separately so applications can retain identity
/// internally without accidentally displaying it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CallInfo {
    pub direction: CallDirection,
    pub calling_name: String,
    pub calling_number: String,
    pub called_name: String,
    pub called_number: String,
    pub original_called_name: String,
    pub original_called_number: String,
    pub last_redirecting_name: String,
    pub last_redirecting_number: String,
    pub original_redirect_reason: u32,
    pub last_redirect_reason: u32,
    /// Protocol restriction mask; `0xf` suppresses all party presentation.
    pub party_restrictions: u32,
}

impl Default for CallInfo {
    fn default() -> Self {
        Self {
            direction: CallDirection::Outbound,
            calling_name: String::new(),
            calling_number: String::new(),
            called_name: String::new(),
            called_number: String::new(),
            original_called_name: String::new(),
            original_called_number: String::new(),
            last_redirecting_name: String::new(),
            last_redirecting_number: String::new(),
            original_redirect_reason: 0,
            last_redirect_reason: 0,
            party_restrictions: 0,
        }
    }
}

/// Default audio packetization interval, in milliseconds.
pub const DEFAULT_AUDIO_PACKET_MS: u32 = 20;
pub const DEFAULT_AUDIO_MAX_FRAMES_PER_PACKET: u32 = 0;

/// Per-appearance station audio processing sent on the receive and transmit
/// channel setup messages.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioProcessingPolicy {
    pub echo_cancellation: EchoCancellation,
    pub silence_suppression: SilenceSuppression,
}

impl Default for AudioProcessingPolicy {
    fn default() -> Self {
        Self {
            echo_cancellation: EchoCancellation::On,
            silence_suppression: SilenceSuppression::Off,
        }
    }
}

/// RTP/RTCP endpoint and negotiated audio format for one media leg.
///
/// Addresses may be IPv4 or IPv6. Ports are host-order values; the message
/// codec performs any wire conversion required by the selected layout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MediaEndpoint {
    pub address: IpAddr,
    pub rtp_port: u16,
    pub rtcp_port: u16,
    pub codec: Codec,
    pub packet_ms: u32,
    pub max_frames_per_packet: u32,
    /// Negotiated RTP payload number for telephone events; zero disables it.
    pub telephone_event_payload: u8,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_explicit_and_lossless() {
        let reference = CallReference::new(42);
        assert_eq!(reference.get(), 42);
        assert_eq!(u32::from(reference), 42);

        let appearance = AppearanceId::new(7);
        assert_eq!(appearance.get(), 7);
        assert_eq!(u32::from(appearance), 7);

        let conference = ConferenceId::new(9);
        assert_eq!(conference.get(), 9);

        let participant = ParticipantId::new(11);
        assert_eq!(participant.get(), 11);
    }

    #[test]
    fn device_id_is_canonicalized() {
        let id: DeviceId = " sep001122334455 ".parse().unwrap();
        assert_eq!(id.as_str(), "SEP001122334455");
        assert_eq!(id.as_ref(), "SEP001122334455");

        let mut devices = HashMap::new();
        devices.insert(id, "desk");
        assert_eq!(devices.get("SEP001122334455"), Some(&"desk"));
    }

    #[test]
    fn registration_selects_the_report_matching_the_effective_peer_family() {
        let registration = DeviceRegistration {
            id: DeviceId::new("SEP001122334455").unwrap(),
            peer: "[2001:db8::20]:2000".parse().unwrap(),
            transport: StationTransport::Clear,
            reported_address: Some("192.0.2.20".parse().unwrap()),
            reported_ipv6_address: Some("2001:db8::20".parse().unwrap()),
            device_type: DeviceType::Cisco7962,
            protocol: ProtocolVersion::V22,
            firmware: "test".into(),
        };
        assert_eq!(
            registration.reported_address_for_peer(),
            Some("2001:db8::20".parse().unwrap())
        );

        let mapped = DeviceRegistration {
            peer: "[::ffff:192.0.2.20]:2000".parse().unwrap(),
            ..registration
        };
        assert_eq!(
            mapped.reported_address_for_peer(),
            Some("192.0.2.20".parse().unwrap())
        );
    }

    fn line_button(instance: u32, number: &str) -> ButtonDefinition {
        ButtonDefinition::Line(LineAppearance::new(
            instance,
            LineDefinition {
                number: number.into(),
                display_name: number.into(),
            },
        ))
    }

    #[test]
    fn station_definition_accepts_non_line_buttons_between_lines() {
        let definition = DeviceDefinition {
            id: DeviceId::new("SEP001122334455").unwrap(),
            description: "Desk".into(),
            transport: StationTransportRequirement::Either,
            signaling_qos: None,
            buttons: vec![
                line_button(1, "1001"),
                ButtonDefinition::Unused,
                ButtonDefinition::SpeedDial(SpeedDialDefinition {
                    instance: 1,
                    number: "2001".into(),
                    display_name: "Warehouse".into(),
                }),
                ButtonDefinition::BlfSpeedDial(BlfSpeedDialDefinition {
                    instance: 1,
                    number: "2002".into(),
                    display_name: "Dispatch".into(),
                }),
                ButtonDefinition::Feature(FeatureDefinition {
                    instance: 2,
                    label: "DND".into(),
                    feature: crate::message::values::ButtonType::DoNotDisturb,
                }),
                ButtonDefinition::Service(ServiceDefinition {
                    instance: 1,
                    label: "Directory".into(),
                    url: "http://pbx.test/directory".into(),
                }),
                ButtonDefinition::AddonModule(AddonModuleDefinition {
                    slot: 1,
                    device_type: crate::message::values::DeviceType::CiscoAddon7914,
                }),
                line_button(2, "1002"),
            ],
            soft_keys: SoftKeyProfile::default(),
            ui: StationUiPolicy::default(),
        };

        definition.validate().unwrap();
        assert_eq!(definition.line_count(), 2);
        assert_eq!(definition.line(2).unwrap().number, "1002");
    }

    #[test]
    fn station_definition_rejects_invalid_signaling_markings() {
        let mut definition = DeviceDefinition {
            id: DeviceId::new("SEP001122334455").unwrap(),
            description: "Desk".into(),
            transport: StationTransportRequirement::Either,
            signaling_qos: Some(SignalingQos::new(64, 0)),
            buttons: vec![line_button(1, "1001")],
            soft_keys: SoftKeyProfile::default(),
            ui: StationUiPolicy::default(),
        };

        assert!(matches!(
            definition.validate(),
            Err(CodecError::InvalidDefinition(message)) if message.contains("DSCP 64")
        ));

        definition.signaling_qos = Some(SignalingQos::new(26, 8));
        assert!(matches!(
            definition.validate(),
            Err(CodecError::InvalidDefinition(message)) if message.contains("COS 8")
        ));
    }

    #[test]
    fn line_appearance_keeps_logical_and_device_specific_state_separate() {
        let logical = LineDefinition {
            number: "1001".into(),
            display_name: "Reception".into(),
        };
        let mut appearance = LineAppearance::new(2, logical.clone());
        appearance.label = Some("Private key".into());
        appearance.caller_id = CallerIdOverride {
            name: Some("Private desk".into()),
            number: None,
        };
        appearance.ring_mode = AppearanceRingMode::Silent;
        appearance.subscription_identity = Some("1001@internal".into());
        appearance.privacy = true;

        assert_eq!(appearance.line, logical);
        assert_eq!(appearance.display_label(), "Private key");
        assert_eq!(appearance.number, "1001");
        assert_eq!(appearance.id, AppearanceId::new(2));
    }

    #[test]
    fn station_definition_rejects_zero_and_duplicate_typed_instances() {
        let definition = DeviceDefinition {
            id: DeviceId::new("SEP001122334455").unwrap(),
            description: "Desk".into(),
            transport: StationTransportRequirement::Either,
            signaling_qos: None,
            buttons: vec![line_button(1, "1001"), line_button(1, "1002")],
            soft_keys: SoftKeyProfile::default(),
            ui: StationUiPolicy::default(),
        };
        assert!(matches!(
            definition.validate(),
            Err(CodecError::InvalidDefinition(message))
                if message.contains("repeats line button instance 1")
        ));

        let definition = DeviceDefinition {
            id: DeviceId::new("SEP001122334455").unwrap(),
            description: "Desk".into(),
            transport: StationTransportRequirement::Either,
            signaling_qos: None,
            buttons: vec![
                line_button(1, "1001"),
                ButtonDefinition::Feature(FeatureDefinition {
                    instance: 0,
                    label: "DND".into(),
                    feature: crate::message::values::ButtonType::DoNotDisturb,
                }),
            ],
            soft_keys: SoftKeyProfile::default(),
            ui: StationUiPolicy::default(),
        };
        assert!(matches!(
            definition.validate(),
            Err(CodecError::InvalidDefinition(message))
                if message.contains("feature button with instance zero")
        ));

        let definition = DeviceDefinition {
            id: DeviceId::new("SEP001122334455").unwrap(),
            description: "Desk".into(),
            transport: StationTransportRequirement::Either,
            signaling_qos: None,
            buttons: vec![
                line_button(1, "1001"),
                ButtonDefinition::Feature(FeatureDefinition {
                    instance: 1,
                    label: "DND".into(),
                    feature: crate::message::values::ButtonType::DoNotDisturb,
                }),
                ButtonDefinition::BlfSpeedDial(BlfSpeedDialDefinition {
                    instance: 1,
                    number: "2001".into(),
                    display_name: "Warehouse".into(),
                }),
            ],
            soft_keys: SoftKeyProfile::default(),
            ui: StationUiPolicy::default(),
        };
        assert!(matches!(
            definition.validate(),
            Err(CodecError::InvalidDefinition(message))
                if message.contains("repeats feature button instance 1")
        ));

        let distinct_namespaces = DeviceDefinition {
            id: DeviceId::new("SEP001122334455").unwrap(),
            description: "Desk".into(),
            transport: StationTransportRequirement::Either,
            signaling_qos: None,
            buttons: vec![
                line_button(1, "1001"),
                ButtonDefinition::SpeedDial(SpeedDialDefinition {
                    instance: 7,
                    number: "2001".into(),
                    display_name: "Warehouse".into(),
                }),
                ButtonDefinition::BlfSpeedDial(BlfSpeedDialDefinition {
                    instance: 7,
                    number: "2002".into(),
                    display_name: "Dispatch".into(),
                }),
            ],
            soft_keys: SoftKeyProfile::default(),
            ui: StationUiPolicy::default(),
        };
        distinct_namespaces.validate().unwrap();
    }

    #[test]
    fn station_definition_enforces_one_byte_wire_instances_for_each_button_family() {
        let definition_with = |button| DeviceDefinition {
            id: DeviceId::new("SEP001122334455").unwrap(),
            description: "Desk".into(),
            transport: StationTransportRequirement::Either,
            signaling_qos: None,
            buttons: vec![line_button(1, "1001"), button],
            soft_keys: SoftKeyProfile::default(),
            ui: StationUiPolicy::default(),
        };
        let buttons = |instance| {
            [
                ButtonDefinition::SpeedDial(SpeedDialDefinition {
                    instance,
                    number: "2001".into(),
                    display_name: "Speed".into(),
                }),
                ButtonDefinition::BlfSpeedDial(BlfSpeedDialDefinition {
                    instance,
                    number: "2002".into(),
                    display_name: "BLF".into(),
                }),
                ButtonDefinition::Feature(FeatureDefinition {
                    instance,
                    label: "DND".into(),
                    feature: crate::message::values::ButtonType::DoNotDisturb,
                }),
                ButtonDefinition::Recording(RecordingButtonDefinition {
                    instance,
                    label: "Record calls".into(),
                }),
                ButtonDefinition::Service(ServiceDefinition {
                    instance,
                    label: "Directory".into(),
                    url: "https://pbx.example/directory".into(),
                }),
            ]
        };

        for button in buttons(255) {
            definition_with(button).validate().unwrap();
        }
        for button in buttons(256) {
            assert!(matches!(
                definition_with(button).validate(),
                Err(CodecError::InvalidDefinition(message))
                    if message.contains("maximum wire instance is 255")
            ));
        }

        let line_255 = DeviceDefinition {
            buttons: vec![line_button(255, "1001")],
            ..definition_with(ButtonDefinition::Unused)
        };
        // Sparse line slots are valid for Extension Mobility layouts.
        let mut line_255 = line_255;
        line_255.buttons.insert(
            0,
            ButtonDefinition::Feature(FeatureDefinition {
                instance: 1,
                label: "Mobility".into(),
                feature: crate::message::values::ButtonType::Mobility,
            }),
        );
        line_255.validate().unwrap();
        line_255.buttons[1] = line_button(256, "1001");
        assert!(matches!(
            line_255.validate(),
            Err(CodecError::InvalidDefinition(message))
                if message.contains("maximum wire instance is 255")
        ));
    }

    #[test]
    fn station_definition_rejects_recording_labels_that_cannot_fit_legacy_status() {
        let definition_with_label = |label: String| DeviceDefinition {
            id: DeviceId::new("SEP001122334455").unwrap(),
            description: "Desk".into(),
            transport: StationTransportRequirement::Either,
            signaling_qos: None,
            buttons: vec![
                line_button(1, "1001"),
                ButtonDefinition::Recording(RecordingButtonDefinition { instance: 1, label }),
            ],
            soft_keys: SoftKeyProfile::default(),
            ui: StationUiPolicy::default(),
        };

        definition_with_label("R".repeat(MAX_STATION_FEATURE_LABEL_BYTES))
            .validate()
            .unwrap();
        for label in [
            String::new(),
            "bad\nlabel".into(),
            "R".repeat(MAX_STATION_FEATURE_LABEL_BYTES + 1),
        ] {
            assert!(matches!(
                definition_with_label(label).validate(),
                Err(CodecError::InvalidDefinition(message))
                    if message.contains("invalid recording-button label")
            ));
        }
    }

    #[test]
    fn station_definition_enforces_station_header_text_contract() {
        let definition_with_description = |description: String| DeviceDefinition {
            id: DeviceId::new("SEP001122334455").unwrap(),
            description,
            transport: StationTransportRequirement::Either,
            signaling_qos: None,
            buttons: vec![line_button(1, "1001")],
            soft_keys: SoftKeyProfile::default(),
            ui: StationUiPolicy::default(),
        };

        for description in [String::new(), "D".repeat(MAX_STATION_HEADER_BYTES)] {
            definition_with_description(description).validate().unwrap();
        }
        for description in [
            "bad\nheader".into(),
            "D".repeat(MAX_STATION_HEADER_BYTES + 1),
        ] {
            assert!(matches!(
                definition_with_description(description).validate(),
                Err(CodecError::InvalidDefinition(message))
                    if message.contains("invalid station-header description")
            ));
        }
    }

    #[test]
    fn blf_defaults_to_unknown() {
        assert_eq!(BlfState::default(), BlfState::Unknown);
    }

    #[test]
    fn station_definition_enforces_bounded_logical_button_limit() {
        let definition = DeviceDefinition {
            id: DeviceId::new("SEP001122334455").unwrap(),
            description: "Desk".into(),
            transport: StationTransportRequirement::Either,
            signaling_qos: None,
            buttons: std::iter::once(line_button(1, "1001"))
                .chain(std::iter::repeat_n(ButtonDefinition::Unused, 256))
                .collect(),
            soft_keys: SoftKeyProfile::default(),
            ui: StationUiPolicy::default(),
        };
        assert!(matches!(
            definition.validate(),
            Err(CodecError::InvalidDefinition(message))
                if message.contains("logical layout limit is 256")
        ));
    }

    #[test]
    fn service_urls_require_bounded_http_parameters() {
        let service_device = |url: &str| DeviceDefinition {
            id: DeviceId::new("SEP001122334455").unwrap(),
            description: "Desk".into(),
            transport: StationTransportRequirement::Either,
            signaling_qos: None,
            buttons: vec![
                line_button(1, "1001"),
                ButtonDefinition::Service(ServiceDefinition {
                    instance: 1,
                    label: "Directory".into(),
                    url: url.into(),
                }),
            ],
            soft_keys: SoftKeyProfile::default(),
            ui: StationUiPolicy::default(),
        };

        service_device("https://pbx.example/sccp/directory?q=Fran%C3%A7ois&page=2")
            .validate()
            .unwrap();
        service_device("https://user:secret@pbx.example/service")
            .validate()
            .unwrap();
        for invalid in [
            "file:///etc/passwd",
            "https://pbx.example/service#private",
            "https://pbx.example/service?=missing-name",
            "not a URL",
        ] {
            let error = service_device(invalid).validate().unwrap_err().to_string();
            assert!(!error.contains(invalid));
        }
        let excessive = format!(
            "https://pbx.example/service?{}",
            (0..33)
                .map(|index| format!("p{index}=v"))
                .collect::<Vec<_>>()
                .join("&")
        );
        assert!(service_device(&excessive).validate().is_err());
    }

    #[test]
    fn soft_key_profiles_require_every_mode_and_unique_known_actions() {
        assert!(matches!(
            SoftKeyProfile::new([(KeyMode::OnHook, vec![SoftKey::NewCall])]),
            Err(CodecError::InvalidDefinition(message))
                if message.contains("every known key mode")
        ));

        let duplicate = SoftKeyProfile::new(KeyMode::ALL_KNOWN.iter().copied().map(|mode| {
            (
                mode,
                if mode == KeyMode::Connected {
                    vec![SoftKey::Hold, SoftKey::Hold]
                } else {
                    Vec::new()
                },
            )
        }));
        assert!(matches!(
            duplicate,
            Err(CodecError::InvalidDefinition(message)) if message.contains("repeats action")
        ));
    }
}
