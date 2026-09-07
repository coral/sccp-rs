//! Typed, bounded management events for live driver transitions.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use sccp_protocol::{
    AlarmSeverity, CallId, CallState, DeviceId, DeviceRegistration, MediaStatus, PhoneAlarmKind,
    PhoneAlarmSummary,
};
use thiserror::Error;

use crate::ami::manager::{
    ManagerBackend, ManagerError, ManagerEvent, ManagerField, ManagerLimits, ManagerPrivilege,
};
use crate::runtime::backend::{
    ManagementEvent, ManagementEventKind, ManagementField, ManagementValue,
};
use crate::runtime::controller::{DeviceFeatureState, DndMode};

pub const REGISTRATION_EVENT: &str = "SCCPRegistration";
pub const ALARM_EVENT: &str = "SCCPAlarm";
pub const FEATURE_EVENT: &str = "SCCPFeature";
pub const MEDIA_EVENT: &str = "SCCPMedia";
pub const CALL_EVENT: &str = "SCCPCall";

const MAX_EVENT_FIELDS: usize = 16;
const MAX_FIELD_VALUE_BYTES: usize = 1024;
const MAX_EVENT_BYTES: usize = 8 * 1024;

pub(crate) const EVENT_LIMITS: ManagerLimits = ManagerLimits {
    max_fields: MAX_EVENT_FIELDS,
    max_field_name_bytes: 64,
    max_field_value_bytes: MAX_FIELD_VALUE_BYTES,
    max_response_bytes: MAX_EVENT_BYTES,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistrationStatus {
    Registered,
    Disconnected,
}

impl RegistrationStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Registered => "registered",
            Self::Disconnected => "disconnected",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FeatureChange {
    Dnd(DndMode),
    DevicePrivacy(bool),
    CallPrivacy { call_id: CallId, enabled: bool },
    ForwardAll(bool),
    ForwardBusy(bool),
    ForwardNoAnswer(bool),
    Button { instance: u32, enabled: bool },
}

/// Return committed feature deltas in stable user-visible order.
pub fn feature_changes(
    previous: &DeviceFeatureState,
    current: &DeviceFeatureState,
) -> Vec<FeatureChange> {
    let mut changes = Vec::new();
    if previous.dnd != current.dnd {
        changes.push(FeatureChange::Dnd(current.dnd));
    }
    if previous.privacy != current.privacy {
        changes.push(FeatureChange::DevicePrivacy(current.privacy));
    }
    if previous.forwarding.all.is_some() != current.forwarding.all.is_some() {
        changes.push(FeatureChange::ForwardAll(current.forwarding.all.is_some()));
    }
    if previous.forwarding.busy.is_some() != current.forwarding.busy.is_some() {
        changes.push(FeatureChange::ForwardBusy(
            current.forwarding.busy.is_some(),
        ));
    }
    if previous.forwarding.no_answer.is_some() != current.forwarding.no_answer.is_some() {
        changes.push(FeatureChange::ForwardNoAnswer(
            current.forwarding.no_answer.is_some(),
        ));
    }
    let instances = previous
        .buttons
        .keys()
        .chain(current.buttons.keys())
        .copied()
        .collect::<BTreeSet<_>>();
    changes.extend(instances.into_iter().filter_map(|instance| {
        let previous = previous.buttons.get(&instance).copied().unwrap_or(false);
        let current = current.buttons.get(&instance).copied().unwrap_or(false);
        (previous != current).then_some(FeatureChange::Button {
            instance,
            enabled: current,
        })
    }));
    changes
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaKind {
    Audio,
    Video,
}

impl MediaKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Audio => "audio",
            Self::Video => "video",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaDirection {
    Receive,
    Transmit,
}

impl MediaDirection {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Receive => "receive",
            Self::Transmit => "transmit",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaState {
    Opening,
    Open,
    Failed,
    Closed,
}

impl MediaState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Opening => "opening",
            Self::Open => "open",
            Self::Failed => "failed",
            Self::Closed => "closed",
        }
    }
}

pub fn registration_event(
    device_id: &DeviceId,
    status: RegistrationStatus,
    registration: Option<&DeviceRegistration>,
) -> ManagementEvent {
    let mut fields = vec![
        text("DeviceId", device_id.as_str()),
        text("Status", status.as_str()),
    ];
    if let Some(registration) = registration {
        fields.extend([
            text("Protocol", registration.protocol.to_string()),
            text("Model", format!("{:?}", registration.device_type)),
            unsigned("ModelId", u64::from(registration.device_type.wire_value())),
            text("Address", registration.peer.to_string()),
        ]);
    }
    ManagementEvent {
        kind: ManagementEventKind::Registration,
        fields,
    }
}

pub fn alarm_event(device_id: &DeviceId, severity: AlarmSeverity) -> ManagementEvent {
    ManagementEvent {
        kind: ManagementEventKind::Alarm,
        fields: vec![
            text("DeviceId", device_id.as_str()),
            text("Source", "protocol"),
            text("Severity", alarm_severity(severity)),
            unsigned("SeverityCode", u64::from(severity.wire_value())),
        ],
    }
}

pub fn xml_alarm_event(device_id: &DeviceId, summary: PhoneAlarmSummary) -> ManagementEvent {
    let mut fields = vec![
        text("DeviceId", device_id.as_str()),
        text("Source", "xml"),
        text(
            "Kind",
            match summary.kind {
                PhoneAlarmKind::LastOutOfService => "last-out-of-service",
            },
        ),
    ];
    if let Some(reason) = summary.reason_for_out_of_service {
        fields.push(signed("ReasonForOutOfService", i64::from(reason)));
    }
    ManagementEvent {
        kind: ManagementEventKind::Alarm,
        fields,
    }
}

pub fn feature_event(device_id: &DeviceId, change: FeatureChange) -> ManagementEvent {
    let (feature, instance, call_id, enabled, mode) = match change {
        FeatureChange::Dnd(mode) => (
            "dnd",
            None,
            None,
            mode != DndMode::Off,
            Some(dnd_mode(mode)),
        ),
        FeatureChange::DevicePrivacy(enabled) => ("privacy", None, None, enabled, None),
        FeatureChange::CallPrivacy { call_id, enabled } => {
            ("call-privacy", None, Some(call_id), enabled, None)
        }
        FeatureChange::ForwardAll(enabled) => ("forward-all", None, None, enabled, None),
        FeatureChange::ForwardBusy(enabled) => ("forward-busy", None, None, enabled, None),
        FeatureChange::ForwardNoAnswer(enabled) => ("forward-no-answer", None, None, enabled, None),
        FeatureChange::Button { instance, enabled } => {
            ("button", Some(instance), None, enabled, None)
        }
    };
    let mut fields = vec![
        text("DeviceId", device_id.as_str()),
        text("Feature", feature),
    ];
    if let Some(instance) = instance {
        fields.push(unsigned("Instance", u64::from(instance)));
    }
    if let Some(call_id) = call_id {
        fields.push(unsigned("CallId", call_id.0));
    }
    fields.push(boolean("Enabled", enabled));
    if let Some(mode) = mode {
        fields.push(text("Mode", mode));
    }
    ManagementEvent {
        kind: ManagementEventKind::Feature,
        fields,
    }
}

#[allow(clippy::too_many_arguments)]
pub fn media_event(
    device_id: &DeviceId,
    call_id: CallId,
    kind: MediaKind,
    direction: MediaDirection,
    state: MediaState,
    status: MediaStatus,
    codec_id: Option<u32>,
    packet_ms: Option<u32>,
) -> ManagementEvent {
    let mut fields = vec![
        text("DeviceId", device_id.as_str()),
        unsigned("CallId", call_id.0),
        text("Kind", kind.as_str()),
        text("Direction", direction.as_str()),
        text("State", state.as_str()),
        text("Status", media_status(status)),
        unsigned("StatusCode", u64::from(status.wire_value())),
    ];
    if let Some(codec_id) = codec_id {
        fields.push(unsigned("CodecId", u64::from(codec_id)));
    }
    if let Some(packet_ms) = packet_ms {
        fields.push(unsigned("PacketMs", u64::from(packet_ms)));
    }
    ManagementEvent {
        kind: ManagementEventKind::Media,
        fields,
    }
}

pub fn call_event(
    device_id: &DeviceId,
    call_id: CallId,
    state: CallState,
    privacy: bool,
) -> ManagementEvent {
    ManagementEvent {
        kind: ManagementEventKind::Call,
        fields: vec![
            text("DeviceId", device_id.as_str()),
            unsigned("CallId", call_id.0),
            text("State", call_state(state)),
            unsigned("StateCode", u64::from(state.wire_value())),
            boolean("Privacy", privacy),
        ],
    }
}

pub trait ManagementEventBackend: Send + Sync + 'static {
    fn publish(&self, event: &ManagerEvent, limits: ManagerLimits) -> Result<(), ManagerError>;
}

impl<B: ManagerBackend> ManagementEventBackend for B {
    fn publish(&self, event: &ManagerEvent, limits: ManagerLimits) -> Result<(), ManagerError> {
        ManagerBackend::publish(self, event, limits)
    }
}

pub(crate) struct EventSequence {
    next: u64,
}

impl Default for EventSequence {
    fn default() -> Self {
        Self { next: 1 }
    }
}

impl EventSequence {
    pub(crate) fn prepare(
        &mut self,
        event: &ManagementEvent,
    ) -> Result<(u64, ManagerEvent), AmiEventError> {
        let sequence = self.next;
        let event = build_manager_event(event, sequence)?;
        self.next = sequence
            .checked_add(1)
            .ok_or(AmiEventError::SequenceExhausted)?;
        Ok((sequence, event))
    }
}

struct PublisherState {
    accepting: bool,
    sequence: EventSequence,
}

pub struct AmiEventPublisher<B> {
    backend: B,
    state: Mutex<PublisherState>,
}

impl<B: ManagementEventBackend> AmiEventPublisher<B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            state: Mutex::new(PublisherState {
                accepting: true,
                sequence: EventSequence::default(),
            }),
        }
    }

    /// Serialize event publication so sequence order is also delivery order.
    pub fn publish(&self, event: &ManagementEvent) -> Result<u64, AmiEventError> {
        let mut state = self.state.lock().map_err(|_| AmiEventError::Unavailable)?;
        if !state.accepting {
            return Err(AmiEventError::Closed);
        }
        let (sequence, event) = state.sequence.prepare(event)?;
        self.backend.publish(&event, EVENT_LIMITS)?;
        Ok(sequence)
    }

    /// Wait for an in-flight synchronous publication and reject future work.
    pub fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.accepting = false;
        }
    }
}

#[derive(Debug, Error)]
pub enum AmiEventError {
    #[error("management event publisher is unavailable")]
    Unavailable,
    #[error("management event publisher is closed")]
    Closed,
    #[error("management event contains a duplicate field")]
    DuplicateField,
    #[error("management event contains an unknown field")]
    UnknownField,
    #[error("management event is missing a required field")]
    MissingField,
    #[error("management event contains a field with the wrong type")]
    WrongType,
    #[error("management event contains an invalid literal")]
    InvalidLiteral,
    #[error("management event exceeds its field or byte bound")]
    TooLarge,
    #[error("management event sequence is exhausted")]
    SequenceExhausted,
    #[error("management event contains invalid output")]
    InvalidOutput,
    #[error(transparent)]
    Manager(#[from] ManagerError),
}

#[derive(Clone, Copy)]
enum ValueKind {
    Text,
    Signed,
    Unsigned,
    Boolean,
}

#[derive(Clone, Copy)]
struct FieldSpec {
    name: &'static str,
    kind: ValueKind,
    required: bool,
}

const REGISTRATION_FIELDS: &[FieldSpec] = &[
    required("DeviceId", ValueKind::Text),
    required("Status", ValueKind::Text),
    optional("Protocol", ValueKind::Text),
    optional("Model", ValueKind::Text),
    optional("ModelId", ValueKind::Unsigned),
    optional("Address", ValueKind::Text),
];
const ALARM_FIELDS: &[FieldSpec] = &[
    required("DeviceId", ValueKind::Text),
    required("Source", ValueKind::Text),
    optional("Severity", ValueKind::Text),
    optional("SeverityCode", ValueKind::Unsigned),
    optional("Kind", ValueKind::Text),
    optional("ReasonForOutOfService", ValueKind::Signed),
];
const FEATURE_FIELDS: &[FieldSpec] = &[
    required("DeviceId", ValueKind::Text),
    required("Feature", ValueKind::Text),
    optional("Instance", ValueKind::Unsigned),
    optional("CallId", ValueKind::Unsigned),
    required("Enabled", ValueKind::Boolean),
    optional("Mode", ValueKind::Text),
];
const MEDIA_FIELDS: &[FieldSpec] = &[
    required("DeviceId", ValueKind::Text),
    required("CallId", ValueKind::Unsigned),
    required("Kind", ValueKind::Text),
    required("Direction", ValueKind::Text),
    required("State", ValueKind::Text),
    required("Status", ValueKind::Text),
    required("StatusCode", ValueKind::Unsigned),
    optional("CodecId", ValueKind::Unsigned),
    optional("PacketMs", ValueKind::Unsigned),
];
const CALL_FIELDS: &[FieldSpec] = &[
    required("DeviceId", ValueKind::Text),
    required("CallId", ValueKind::Unsigned),
    required("State", ValueKind::Text),
    required("StateCode", ValueKind::Unsigned),
    required("Privacy", ValueKind::Boolean),
];

const fn required(name: &'static str, kind: ValueKind) -> FieldSpec {
    FieldSpec {
        name,
        kind,
        required: true,
    }
}

const fn optional(name: &'static str, kind: ValueKind) -> FieldSpec {
    FieldSpec {
        name,
        kind,
        required: false,
    }
}

pub(crate) fn build_manager_event(
    event: &ManagementEvent,
    sequence: u64,
) -> Result<ManagerEvent, AmiEventError> {
    if event.fields.len() + 1 > MAX_EVENT_FIELDS {
        return Err(AmiEventError::TooLarge);
    }
    let (name, category, schema) = event_contract(event.kind);
    let mut provided = BTreeMap::new();
    for field in &event.fields {
        let normalized = field.name.to_ascii_lowercase();
        if provided.insert(normalized, &field.value).is_some() {
            return Err(AmiEventError::DuplicateField);
        }
    }
    let allowed = schema
        .iter()
        .map(|field| field.name.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    if provided.keys().any(|field| !allowed.contains(field)) {
        return Err(AmiEventError::UnknownField);
    }
    validate_field_relationships(event.kind, &provided)?;

    let mut fields = vec![manager_public("Sequence", sequence)?];
    for spec in schema {
        let value = provided.get(&spec.name.to_ascii_lowercase()).copied();
        let Some(value) = value else {
            if spec.required {
                return Err(AmiEventError::MissingField);
            }
            continue;
        };
        validate_value_kind(value, spec.kind)?;
        validate_literal(event.kind, spec.name, value)?;
        fields.push(manager_field(spec.name, value)?);
    }
    validate_event_bounds(name, &fields)?;
    ManagerEvent::new(category, name, fields).map_err(AmiEventError::Manager)
}

fn event_contract(
    kind: ManagementEventKind,
) -> (&'static str, ManagerPrivilege, &'static [FieldSpec]) {
    match kind {
        ManagementEventKind::Registration => (
            REGISTRATION_EVENT,
            ManagerPrivilege::SYSTEM.union(ManagerPrivilege::CALL),
            REGISTRATION_FIELDS,
        ),
        ManagementEventKind::Alarm => (
            ALARM_EVENT,
            ManagerPrivilege::SYSTEM.union(ManagerPrivilege::REPORTING),
            ALARM_FIELDS,
        ),
        ManagementEventKind::Feature => (
            FEATURE_EVENT,
            ManagerPrivilege::CALL.union(ManagerPrivilege::REPORTING),
            FEATURE_FIELDS,
        ),
        ManagementEventKind::Media => (
            MEDIA_EVENT,
            ManagerPrivilege::CALL.union(ManagerPrivilege::REPORTING),
            MEDIA_FIELDS,
        ),
        ManagementEventKind::Call => (
            CALL_EVENT,
            ManagerPrivilege::CALL.union(ManagerPrivilege::REPORTING),
            CALL_FIELDS,
        ),
    }
}

fn validate_value_kind(value: &ManagementValue, kind: ValueKind) -> Result<(), AmiEventError> {
    if matches!(value, ManagementValue::Redacted)
        || matches!(
            (value, kind),
            (ManagementValue::Text(_), ValueKind::Text)
                | (ManagementValue::Signed(_), ValueKind::Signed)
                | (ManagementValue::Unsigned(_), ValueKind::Unsigned)
                | (ManagementValue::Boolean(_), ValueKind::Boolean)
        )
    {
        Ok(())
    } else {
        Err(AmiEventError::WrongType)
    }
}

fn validate_literal(
    kind: ManagementEventKind,
    name: &str,
    value: &ManagementValue,
) -> Result<(), AmiEventError> {
    let ManagementValue::Text(value) = value else {
        return Ok(());
    };
    let accepted = match (kind, name) {
        (ManagementEventKind::Registration, "Status") => {
            matches!(value.as_str(), "registered" | "disconnected")
        }
        (ManagementEventKind::Feature, "Feature") => matches!(
            value.as_str(),
            "dnd"
                | "privacy"
                | "call-privacy"
                | "forward-all"
                | "forward-busy"
                | "forward-no-answer"
                | "button"
        ),
        (ManagementEventKind::Feature, "Mode") => {
            matches!(value.as_str(), "off" | "silent" | "reject")
        }
        (ManagementEventKind::Alarm, "Severity") => matches!(
            value.as_str(),
            "critical"
                | "warning"
                | "informational"
                | "protocol-unknown"
                | "major"
                | "minor"
                | "marginal"
                | "trace-info"
                | "unknown"
        ),
        (ManagementEventKind::Alarm, "Source") => matches!(value.as_str(), "protocol" | "xml"),
        (ManagementEventKind::Alarm, "Kind") => value == "last-out-of-service",
        (ManagementEventKind::Media, "Kind") => matches!(value.as_str(), "audio" | "video"),
        (ManagementEventKind::Media, "Direction") => {
            matches!(value.as_str(), "receive" | "transmit")
        }
        (ManagementEventKind::Media, "State") => {
            matches!(value.as_str(), "opening" | "open" | "failed" | "closed")
        }
        (ManagementEventKind::Media, "Status") => matches!(
            value.as_str(),
            "ok" | "unspecified-error"
                | "out-of-channels"
                | "codec-too-complex"
                | "invalid-party-id"
                | "invalid-call-reference"
                | "invalid-codec"
                | "invalid-packet-size"
                | "out-of-sockets"
                | "encoder-or-decoder-failed"
                | "invalid-dynamic-payload"
                | "address-type-unavailable"
                | "device-on-hook"
                | "unknown"
        ),
        (ManagementEventKind::Call, "State") => matches!(
            value.as_str(),
            "off-hook"
                | "on-hook"
                | "ring-out"
                | "ring-in"
                | "connected"
                | "busy"
                | "congestion"
                | "hold"
                | "call-waiting"
                | "transfer"
                | "park"
                | "proceed"
                | "remote-multiline"
                | "invalid-number"
                | "hold-yellow"
                | "intercom-one-way"
                | "hold-red"
                | "unknown"
        ),
        _ => true,
    };
    if accepted {
        Ok(())
    } else {
        Err(AmiEventError::InvalidLiteral)
    }
}

fn validate_field_relationships(
    kind: ManagementEventKind,
    fields: &BTreeMap<String, &ManagementValue>,
) -> Result<(), AmiEventError> {
    let has = |name: &str| fields.contains_key(name);
    let text = |name: &str| match fields.get(name) {
        Some(ManagementValue::Text(value)) => Some(value.as_str()),
        _ => None,
    };
    let valid = match kind {
        ManagementEventKind::Registration => match text("status") {
            Some("registered") => {
                has("protocol") && has("model") && has("modelid") && has("address")
            }
            Some("disconnected") => {
                !has("protocol") && !has("model") && !has("modelid") && !has("address")
            }
            _ => true,
        },
        ManagementEventKind::Feature => match text("feature") {
            Some("dnd") => has("mode") && !has("instance") && !has("callid"),
            Some("button") => has("instance") && !has("mode") && !has("callid"),
            Some("call-privacy") => has("callid") && !has("instance") && !has("mode"),
            Some(_) => !has("instance") && !has("callid") && !has("mode"),
            None => true,
        },
        ManagementEventKind::Media => match text("state") {
            Some("open") => text("status") == Some("ok") && has("codecid") && has("packetms"),
            Some("failed") => {
                text("status").is_some_and(|status| status != "ok")
                    && !has("codecid")
                    && !has("packetms")
            }
            Some("opening" | "closed") => !has("codecid") && !has("packetms"),
            _ => true,
        },
        ManagementEventKind::Alarm => match text("source") {
            Some("protocol") => {
                has("severity")
                    && has("severitycode")
                    && !has("kind")
                    && !has("reasonforoutofservice")
            }
            Some("xml") => {
                has("kind")
                    && !has("severity")
                    && !has("severitycode")
                    && text("kind") == Some("last-out-of-service")
            }
            _ => true,
        },
        ManagementEventKind::Call => true,
    };
    if valid {
        Ok(())
    } else {
        Err(AmiEventError::InvalidLiteral)
    }
}

fn manager_field(
    name: &'static str,
    value: &ManagementValue,
) -> Result<ManagerField, AmiEventError> {
    match value {
        ManagementValue::Text(value) => manager_public(name, value),
        ManagementValue::Signed(value) => manager_public(name, value),
        ManagementValue::Unsigned(value) => manager_public(name, value),
        ManagementValue::Boolean(value) => manager_public(name, yes_no(*value)),
        ManagementValue::Redacted => {
            ManagerField::redacted(name).map_err(|_| AmiEventError::InvalidOutput)
        }
    }
}

fn manager_public(name: &'static str, value: impl ToString) -> Result<ManagerField, AmiEventError> {
    let value = value.to_string();
    if value.len() > MAX_FIELD_VALUE_BYTES {
        return Err(AmiEventError::TooLarge);
    }
    ManagerField::public(name, value).map_err(|_| AmiEventError::InvalidOutput)
}

fn validate_event_bounds(name: &str, fields: &[ManagerField]) -> Result<(), AmiEventError> {
    let mut bytes = name.len();
    for field in fields {
        let value_bytes = field.public_value().map_or("<redacted>".len(), str::len);
        bytes = bytes
            .checked_add(field.name().len())
            .and_then(|bytes| bytes.checked_add(value_bytes))
            .and_then(|bytes| bytes.checked_add(4))
            .ok_or(AmiEventError::TooLarge)?;
    }
    if bytes > MAX_EVENT_BYTES {
        Err(AmiEventError::TooLarge)
    } else {
        Ok(())
    }
}

fn text(name: impl Into<String>, value: impl Into<String>) -> ManagementField {
    ManagementField {
        name: name.into(),
        value: ManagementValue::Text(value.into()),
    }
}

fn unsigned(name: impl Into<String>, value: u64) -> ManagementField {
    ManagementField {
        name: name.into(),
        value: ManagementValue::Unsigned(value),
    }
}

fn signed(name: impl Into<String>, value: i64) -> ManagementField {
    ManagementField {
        name: name.into(),
        value: ManagementValue::Signed(value),
    }
}

fn boolean(name: impl Into<String>, value: bool) -> ManagementField {
    ManagementField {
        name: name.into(),
        value: ManagementValue::Boolean(value),
    }
}

const fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

const fn dnd_mode(value: DndMode) -> &'static str {
    match value {
        DndMode::Off => "off",
        DndMode::Silent => "silent",
        DndMode::Reject => "reject",
    }
}

const fn alarm_severity(value: AlarmSeverity) -> &'static str {
    match value {
        AlarmSeverity::Critical => "critical",
        AlarmSeverity::Warning => "warning",
        AlarmSeverity::Informational => "informational",
        AlarmSeverity::ProtocolUnknown => "protocol-unknown",
        AlarmSeverity::Major => "major",
        AlarmSeverity::Minor => "minor",
        AlarmSeverity::Marginal => "marginal",
        AlarmSeverity::TraceInfo => "trace-info",
        AlarmSeverity::Unknown(_) => "unknown",
    }
}

const fn media_status(value: MediaStatus) -> &'static str {
    match value {
        MediaStatus::Ok => "ok",
        MediaStatus::UnspecifiedError => "unspecified-error",
        MediaStatus::OutOfChannels => "out-of-channels",
        MediaStatus::CodecTooComplex => "codec-too-complex",
        MediaStatus::InvalidPartyId => "invalid-party-id",
        MediaStatus::InvalidCallReference => "invalid-call-reference",
        MediaStatus::InvalidCodec => "invalid-codec",
        MediaStatus::InvalidPacketSize => "invalid-packet-size",
        MediaStatus::OutOfSockets => "out-of-sockets",
        MediaStatus::EncoderOrDecoderFailed => "encoder-or-decoder-failed",
        MediaStatus::InvalidDynamicPayload => "invalid-dynamic-payload",
        MediaStatus::RequestedAddressTypeUnavailable => "address-type-unavailable",
        MediaStatus::DeviceOnHook => "device-on-hook",
        MediaStatus::Unknown(_) => "unknown",
    }
}

const fn call_state(value: CallState) -> &'static str {
    match value {
        CallState::OffHook => "off-hook",
        CallState::OnHook => "on-hook",
        CallState::RingOut => "ring-out",
        CallState::RingIn => "ring-in",
        CallState::Connected => "connected",
        CallState::Busy => "busy",
        CallState::Congestion => "congestion",
        CallState::Hold => "hold",
        CallState::CallWaiting => "call-waiting",
        CallState::Transfer => "transfer",
        CallState::Park => "park",
        CallState::Proceed => "proceed",
        CallState::RemoteMultiline => "remote-multiline",
        CallState::InvalidNumber => "invalid-number",
        CallState::HoldYellow => "hold-yellow",
        CallState::IntercomOneWay => "intercom-one-way",
        CallState::HoldRed => "hold-red",
        CallState::Unknown(_) => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::{Arc, Condvar};
    use std::thread;
    use std::time::Duration;

    use sccp_protocol::{DeviceType, ProtocolVersion};

    use super::*;

    #[derive(Default)]
    struct FakeState {
        events: Vec<ManagerEvent>,
        fail: bool,
    }

    #[derive(Clone, Default)]
    struct FakeBackend {
        state: Arc<Mutex<FakeState>>,
        gate: Option<Arc<(Mutex<bool>, Condvar)>>,
    }

    impl ManagementEventBackend for FakeBackend {
        fn publish(
            &self,
            event: &ManagerEvent,
            _limits: ManagerLimits,
        ) -> Result<(), ManagerError> {
            if let Some(gate) = &self.gate {
                let mut released = gate.0.lock().unwrap();
                while !*released {
                    released = gate.1.wait(released).unwrap();
                }
            }
            let mut state = self.state.lock().unwrap();
            if state.fail {
                Err(ManagerError::PublishFailed)
            } else {
                state.events.push(event.clone());
                Ok(())
            }
        }
    }

    fn device() -> DeviceId {
        DeviceId::new("SEP001122334455").unwrap()
    }

    fn field<'a>(event: &'a ManagerEvent, name: &str) -> Option<&'a ManagerField> {
        event.fields().iter().find(|field| field.name() == name)
    }

    fn registration() -> DeviceRegistration {
        DeviceRegistration {
            id: device(),
            peer: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7)), 2000),
            transport: sccp_protocol::StationTransport::Clear,
            reported_address: None,
            reported_ipv6_address: None,
            device_type: DeviceType::Cisco7962,
            protocol: ProtocolVersion::V17,
            firmware: "private-firmware-token".into(),
        }
    }

    #[test]
    fn all_five_event_families_have_canonical_names_fields_and_categories() {
        let publisher = AmiEventPublisher::new(FakeBackend::default());
        let registration = registration();
        let events = [
            registration_event(
                &registration.id,
                RegistrationStatus::Registered,
                Some(&registration),
            ),
            alarm_event(&device(), AlarmSeverity::Major),
            feature_event(&device(), FeatureChange::Dnd(DndMode::Reject)),
            media_event(
                &device(),
                CallId(44),
                MediaKind::Audio,
                MediaDirection::Receive,
                MediaState::Open,
                MediaStatus::Ok,
                Some(4),
                Some(20),
            ),
            call_event(&device(), CallId(44), CallState::Connected, true),
        ];
        for event in &events {
            publisher.publish(event).unwrap();
        }
        let captured = publisher.backend.state.lock().unwrap();
        assert_eq!(
            captured
                .events
                .iter()
                .map(ManagerEvent::name)
                .collect::<Vec<_>>(),
            [
                REGISTRATION_EVENT,
                ALARM_EVENT,
                FEATURE_EVENT,
                MEDIA_EVENT,
                CALL_EVENT,
            ]
        );
        assert_eq!(
            captured
                .events
                .iter()
                .filter_map(|event| field(event, "Sequence")?.public_value())
                .collect::<Vec<_>>(),
            ["1", "2", "3", "4", "5"]
        );
        assert_eq!(
            field(&captured.events[2], "Mode").and_then(ManagerField::public_value),
            Some("reject")
        );
        assert_eq!(
            field(&captured.events[4], "Privacy").and_then(ManagerField::public_value),
            Some("yes")
        );
        assert!(field(&captured.events[1], "AlarmText").is_none());
        assert!(field(&captured.events[4], "CallingName").is_none());
    }

    #[test]
    fn typed_xml_alarm_publishes_only_allowlisted_summary() {
        let publisher = AmiEventPublisher::new(FakeBackend::default());
        publisher
            .publish(&xml_alarm_event(
                &device(),
                PhoneAlarmSummary {
                    kind: PhoneAlarmKind::LastOutOfService,
                    reason_for_out_of_service: Some(17),
                },
            ))
            .unwrap();
        let captured = publisher.backend.state.lock().unwrap();
        let event = &captured.events[0];
        assert_eq!(
            field(event, "Source").and_then(ManagerField::public_value),
            Some("xml")
        );
        assert_eq!(
            field(event, "Kind").and_then(ManagerField::public_value),
            Some("last-out-of-service")
        );
        assert_eq!(
            field(event, "ReasonForOutOfService").and_then(ManagerField::public_value),
            Some("17")
        );
        assert!(field(event, "Severity").is_none());
        assert!(field(event, "AlarmText").is_none());
    }

    #[test]
    fn event_debug_and_errors_do_not_expose_text_values_or_omitted_secrets() {
        let registration = registration();
        let event = registration_event(
            &registration.id,
            RegistrationStatus::Registered,
            Some(&registration),
        );
        let rendered = format!("{event:?}");
        assert!(!rendered.contains(registration.id.as_str()));
        assert!(!rendered.contains("192.0.2.7"));
        assert!(!rendered.contains("private-firmware-token"));

        let mut unsafe_alarm = alarm_event(&device(), AlarmSeverity::Warning);
        unsafe_alarm.fields[0].value = ManagementValue::Text("private-value\r\ninjected".into());
        let error = AmiEventPublisher::new(FakeBackend::default())
            .publish(&unsafe_alarm)
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "management event contains invalid output"
        );
        assert!(!error.to_string().contains("private-value"));
    }

    #[test]
    fn duplicate_unknown_missing_wrong_type_literal_and_bounds_fail_closed() {
        let publisher = AmiEventPublisher::new(FakeBackend::default());
        let mut duplicate = call_event(&device(), CallId(1), CallState::Connected, false);
        duplicate.fields.push(unsigned("callid", 2));
        assert!(matches!(
            publisher.publish(&duplicate),
            Err(AmiEventError::DuplicateField)
        ));

        let mut unknown = feature_event(&device(), FeatureChange::DevicePrivacy(true));
        unknown.fields.push(text("Credential", "do-not-disclose"));
        assert!(matches!(
            publisher.publish(&unknown),
            Err(AmiEventError::UnknownField)
        ));

        let missing = ManagementEvent {
            kind: ManagementEventKind::Call,
            fields: Vec::new(),
        };
        assert!(matches!(
            publisher.publish(&missing),
            Err(AmiEventError::MissingField)
        ));

        let mut wrong = call_event(&device(), CallId(1), CallState::Connected, false);
        wrong
            .fields
            .iter_mut()
            .find(|field| field.name == "CallId")
            .unwrap()
            .value = ManagementValue::Text("1".into());
        assert!(matches!(
            publisher.publish(&wrong),
            Err(AmiEventError::WrongType)
        ));

        let mut literal = registration_event(&device(), RegistrationStatus::Disconnected, None);
        literal
            .fields
            .iter_mut()
            .find(|field| field.name == "Status")
            .unwrap()
            .value = ManagementValue::Text("invented".into());
        assert!(matches!(
            publisher.publish(&literal),
            Err(AmiEventError::InvalidLiteral)
        ));

        let mut oversized = alarm_event(&device(), AlarmSeverity::Warning);
        oversized.fields[0].value = ManagementValue::Text("x".repeat(MAX_FIELD_VALUE_BYTES + 1));
        assert!(matches!(
            publisher.publish(&oversized),
            Err(AmiEventError::TooLarge)
        ));
    }

    #[test]
    fn feature_deltas_are_deduplicated_and_ordered_without_destinations() {
        let previous = DeviceFeatureState {
            buttons: [(8, true), (2, false)].into_iter().collect(),
            ..DeviceFeatureState::default()
        };
        let current = DeviceFeatureState {
            dnd: DndMode::Silent,
            privacy: true,
            recording_armed: false,
            forwarding: crate::runtime::controller::ForwardingState {
                all: Some(
                    crate::call::forwarding::ForwardingDestination::new("private-forward").unwrap(),
                ),
                busy: None,
                no_answer: Some(
                    crate::call::forwarding::ForwardingDestination::new("private-no-answer")
                        .unwrap(),
                ),
            },
            buttons: [(2, true), (8, false)].into_iter().collect(),
        };
        let changes = feature_changes(&previous, &current);
        assert_eq!(
            changes,
            [
                FeatureChange::Dnd(DndMode::Silent),
                FeatureChange::DevicePrivacy(true),
                FeatureChange::ForwardAll(true),
                FeatureChange::ForwardNoAnswer(true),
                FeatureChange::Button {
                    instance: 2,
                    enabled: true,
                },
                FeatureChange::Button {
                    instance: 8,
                    enabled: false,
                },
            ]
        );
        let rendered = format!("{changes:?}");
        assert!(!rendered.contains("private-forward-destination"));
        assert!(!rendered.contains("private-no-answer-destination"));
    }

    #[test]
    fn failures_consume_one_ordered_sequence_without_closing_the_publisher() {
        let backend = FakeBackend::default();
        let publisher = AmiEventPublisher::new(backend.clone());
        backend.state.lock().unwrap().fail = true;
        assert!(matches!(
            publisher.publish(&call_event(&device(), CallId(1), CallState::OffHook, false,)),
            Err(AmiEventError::Manager(ManagerError::PublishFailed))
        ));
        backend.state.lock().unwrap().fail = false;
        assert_eq!(
            publisher
                .publish(&call_event(
                    &device(),
                    CallId(1),
                    CallState::Connected,
                    false,
                ))
                .unwrap(),
            2
        );
    }

    #[test]
    fn close_waits_for_inflight_publication_and_rejects_future_events() {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let publisher = Arc::new(AmiEventPublisher::new(FakeBackend {
            gate: Some(Arc::clone(&gate)),
            ..FakeBackend::default()
        }));
        let publishing = {
            let publisher = Arc::clone(&publisher);
            thread::spawn(move || {
                publisher.publish(&call_event(
                    &device(),
                    CallId(7),
                    CallState::Connected,
                    false,
                ))
            })
        };
        thread::sleep(Duration::from_millis(20));
        let (closed_tx, closed_rx) = std::sync::mpsc::channel();
        let closing = {
            let publisher = Arc::clone(&publisher);
            thread::spawn(move || {
                publisher.close();
                closed_tx.send(()).unwrap();
            })
        };
        assert!(closed_rx.recv_timeout(Duration::from_millis(30)).is_err());
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        assert_eq!(publishing.join().unwrap().unwrap(), 1);
        closed_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        closing.join().unwrap();
        assert!(matches!(
            publisher.publish(&call_event(
                &device(),
                CallId(8),
                CallState::Connected,
                false,
            )),
            Err(AmiEventError::Closed)
        ));
    }

    #[test]
    fn development_backend_is_explicitly_unavailable() {
        #[cfg(feature = "development")]
        assert!(matches!(
            AmiEventPublisher::new(crate::ami::manager::UnavailableManager).publish(&call_event(
                &device(),
                CallId(1),
                CallState::Connected,
                false,
            )),
            Err(AmiEventError::Manager(ManagerError::Unavailable))
        ));
    }
}
