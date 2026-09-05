//! Typed management actions for mutable device feature state.
//!
//! `SCCPDeviceSetDND` requires `DeviceName` and `DNDState` (`off`, `silent`, or
//! `reject`). `SCCPLineForwardUpdate` requires `DeviceName`, an exact
//! `LineName`, and `ForwardType` (`all`, `busy`, or `noanswer`). `Disable=yes`
//! clears the selected kind and conflicts with a nonempty `Number`; otherwise a
//! bounded forwarding number is required.
//!
//! Both actions enter the same serialized feature transaction used by handset
//! buttons and CLI forwarding: validate ownership/capability, persist, commit
//! controller state, then publish every affected button/appearance. A store or
//! handset failure rolls back and never reports a false UI state. Destinations
//! are redacted from `Debug` and error output.

use std::collections::BTreeMap;
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
use std::collections::BTreeSet;
use std::fmt;

use sccp_protocol::DeviceId;
use thiserror::Error;

use crate::ami::controls::MAX_DEVICE_SELECTOR_BYTES;
use crate::ami::manager::{
    ActionDefinition, ManagerBackend, ManagerError, ManagerField, ManagerLimits, ManagerPrivilege,
    ManagerRequest, ManagerResponse, RequestFields, register_action_group,
};
use crate::call::forwarding::ForwardingDestination;
pub use crate::call::forwarding::ForwardingKind;
use crate::runtime::controller::DndMode;

pub const SET_DND_ACTION: &str = "SCCPDeviceSetDND";
pub const SET_FORWARDING_ACTION: &str = "SCCPLineForwardUpdate";

const MAX_LINE_SELECTOR_BYTES: usize = 24;
pub const MAX_DND_MODE_BYTES: usize = 6;
const ACTION_LIMITS: ManagerLimits = ManagerLimits {
    max_fields: 8,
    max_field_name_bytes: 64,
    max_field_value_bytes: 256,
    max_response_bytes: 4096,
};
const ACTION_PRIVILEGES: ManagerPrivilege =
    ManagerPrivilege::SYSTEM.union(ManagerPrivilege::CONFIG);

#[derive(Clone, Eq, PartialEq)]
pub enum FeatureControlMutation {
    Dnd {
        device_id: DeviceId,
        mode: DndMode,
    },
    Forwarding {
        device_id: DeviceId,
        line: String,
        kind: ForwardingKind,
        destination: Option<ForwardingDestination>,
    },
}

impl fmt::Debug for FeatureControlMutation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dnd { device_id, mode } => formatter
                .debug_struct("Dnd")
                .field("device_id", device_id)
                .field("mode", mode)
                .finish(),
            Self::Forwarding {
                device_id,
                line,
                kind,
                destination,
            } => formatter
                .debug_struct("Forwarding")
                .field("device_id", device_id)
                .field("line", line)
                .field("kind", kind)
                .field("destination", &destination.as_ref().map(|_| "<redacted>"))
                .finish(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FeatureControlOutcome {
    Dnd {
        device_id: DeviceId,
        mode: DndMode,
        changed: bool,
    },
    Forwarding {
        device_id: DeviceId,
        line: String,
        kind: ForwardingKind,
        enabled: bool,
        changed: bool,
    },
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum FeatureControlProviderError {
    #[error("feature control is unavailable")]
    Unavailable,
    #[error("the requested device does not exist")]
    DeviceNotFound,
    #[error("the requested line is not an appearance on the device")]
    LineNotFound,
    #[error("the requested feature is disabled")]
    FeatureDisabled,
    #[error("feature persistence failed without changing runtime state")]
    Persistence,
    #[error("feature persistence and rollback both failed")]
    PersistenceDiverged,
}

pub trait FeatureControlProvider: Send + Sync + 'static {
    fn apply(
        &self,
        mutation: FeatureControlMutation,
    ) -> Result<FeatureControlOutcome, FeatureControlProviderError>;
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum FeatureCliError {
    #[error("invalid feature device")]
    InvalidDevice,
    #[error("invalid forwarding line selector")]
    InvalidLine,
    #[error("invalid forwarding type")]
    InvalidKind,
    #[error("invalid forwarding destination")]
    InvalidDestination,
    #[error("invalid DND mode")]
    InvalidDndMode,
    #[error(transparent)]
    Provider(#[from] FeatureControlProviderError),
}

pub fn parse_cli_dnd_mutation(
    device: &str,
    mode: &str,
) -> Result<FeatureControlMutation, FeatureCliError> {
    if device.is_empty()
        || device.len() > MAX_DEVICE_SELECTOR_BYTES
        || !device.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        return Err(FeatureCliError::InvalidDevice);
    }
    let device_id = DeviceId::new(device).map_err(|_| FeatureCliError::InvalidDevice)?;
    let mode = parse_dnd(mode).map_err(|_| FeatureCliError::InvalidDndMode)?;
    Ok(FeatureControlMutation::Dnd { device_id, mode })
}

pub fn execute_cli_dnd<P: FeatureControlProvider + ?Sized>(
    provider: &P,
    device: &str,
    mode: &str,
) -> Result<FeatureControlOutcome, FeatureCliError> {
    provider
        .apply(parse_cli_dnd_mutation(device, mode)?)
        .map_err(Into::into)
}

pub fn parse_cli_forwarding_mutation(
    device: &str,
    line: &str,
    kind: &str,
    destination: &str,
) -> Result<FeatureControlMutation, FeatureCliError> {
    let device_id = DeviceId::new(device).map_err(|_| FeatureCliError::InvalidDevice)?;
    let trimmed_line = line.trim();
    if trimmed_line != line
        || line.is_empty()
        || line.len() > MAX_LINE_SELECTOR_BYTES
        || line.chars().any(char::is_control)
    {
        return Err(FeatureCliError::InvalidLine);
    }
    let kind = ForwardingKind::parse(kind).ok_or(FeatureCliError::InvalidKind)?;
    let destination = if destination.eq_ignore_ascii_case("off") {
        None
    } else {
        Some(
            ForwardingDestination::new(destination)
                .map_err(|_| FeatureCliError::InvalidDestination)?,
        )
    };
    Ok(FeatureControlMutation::Forwarding {
        device_id,
        line: line.to_owned(),
        kind,
        destination,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FeatureAction {
    Dnd,
    Forwarding,
}

impl FeatureAction {
    const ALL: [Self; 2] = [Self::Dnd, Self::Forwarding];

    const fn name(self) -> &'static str {
        match self {
            Self::Dnd => SET_DND_ACTION,
            Self::Forwarding => SET_FORWARDING_ACTION,
        }
    }

    const fn synopsis(self) -> &'static str {
        match self {
            Self::Dnd => "Set device DND mode",
            Self::Forwarding => "Set device line forwarding",
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::Dnd => "Set DNDState to off, silent, or reject for one configured device.",
            Self::Forwarding => {
                "Validate Linename as a device appearance, then set or disable the device-wide all, busy, or noanswer forwarding state."
            }
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|action| action.name().eq_ignore_ascii_case(value))
    }
}

impl<P: FeatureControlProvider> ActionDefinition<P> for FeatureAction {
    fn name(self) -> &'static str {
        self.name()
    }

    fn synopsis(self) -> &'static str {
        self.synopsis()
    }

    fn description(self) -> &'static str {
        self.description()
    }

    fn privileges(self) -> ManagerPrivilege {
        ACTION_PRIVILEGES
    }

    fn limits(self) -> ManagerLimits {
        ACTION_LIMITS
    }

    fn handle(self, provider: &P, request: ManagerRequest) -> ManagerResponse {
        handle_feature_control_request(provider, request)
    }
}

/// Register both mutable-feature actions as one RAII-owned lifecycle group.
pub fn register_feature_control_actions<P: FeatureControlProvider, M: ManagerBackend>(
    provider: P,
    manager: M,
) -> Result<Vec<M::Registration>, ManagerError> {
    register_action_group(provider, manager, &FeatureAction::ALL)
}

pub fn handle_feature_control_request<P: FeatureControlProvider + ?Sized>(
    provider: &P,
    request: ManagerRequest,
) -> ManagerResponse {
    match execute_feature_control_request(provider, &request) {
        Ok(outcome) => success_response(outcome),
        Err(error) => ManagerResponse::error(error.response_message())
            .expect("fixed feature-control error is valid"),
    }
}

/// Validate an optional appearance selector and return every handset line
/// instance that must receive the device-wide forwarding state.
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) fn forwarding_ui_line_instances<'a>(
    selected_line: Option<&str>,
    appearances: impl IntoIterator<Item = (&'a str, u32)>,
) -> Option<Vec<u32>> {
    let mut selected = selected_line.is_none();
    let mut instances = BTreeSet::new();
    for (line, instance) in appearances {
        selected |= selected_line.is_some_and(|candidate| candidate == line);
        instances.insert(instance);
    }
    selected.then(|| instances.into_iter().collect())
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
enum FeatureActionError {
    #[error("unknown feature-control action")]
    UnknownAction,
    #[error("request field is not allowlisted")]
    UnknownField,
    #[error("request repeats a singleton field")]
    DuplicateField,
    #[error("request contains sensitive metadata")]
    SensitiveField,
    #[error("request is missing a required field")]
    MissingField,
    #[error("request contains a malformed selector")]
    InvalidSelector,
    #[error("request contains an invalid DND mode")]
    InvalidDndMode,
    #[error("request contains an invalid forwarding type")]
    InvalidForwardingKind,
    #[error("request contains an invalid disable flag")]
    InvalidDisable,
    #[error("forwarding destination is missing, unsafe, or too long")]
    InvalidDestination,
    #[error("disable and destination fields conflict")]
    ConflictingDestination,
    #[error("management response cannot be represented safely")]
    InvalidOutput,
    #[error(transparent)]
    Provider(#[from] FeatureControlProviderError),
}

crate::ami::manager::impl_request_fields_error!(FeatureActionError);

impl FeatureActionError {
    const fn response_message(self) -> &'static str {
        match self {
            Self::UnknownAction => "Unknown feature-control action",
            Self::UnknownField => "Request field is not allowlisted",
            Self::DuplicateField => "Request repeats a singleton field",
            Self::SensitiveField => "Sensitive request fields are not accepted",
            Self::MissingField => "Request is missing a required field",
            Self::InvalidSelector => "Request contains a malformed selector",
            Self::InvalidDndMode => "DNDState must be off, silent, or reject",
            Self::InvalidForwardingKind => "Forwardtype must be all, busy, or noanswer",
            Self::InvalidDisable => "Disable must be a boolean value",
            Self::InvalidDestination => "Number is missing, unsafe, or too long",
            Self::ConflictingDestination => "Number must be omitted when disabling forwarding",
            Self::InvalidOutput => "Feature-control response cannot be represented safely",
            Self::Provider(FeatureControlProviderError::Unavailable) => {
                "Feature control is unavailable"
            }
            Self::Provider(FeatureControlProviderError::DeviceNotFound) => {
                "Requested device was not found"
            }
            Self::Provider(FeatureControlProviderError::LineNotFound) => {
                "Requested line is not an appearance on the device"
            }
            Self::Provider(FeatureControlProviderError::FeatureDisabled) => {
                "Requested feature is disabled"
            }
            Self::Provider(FeatureControlProviderError::Persistence) => {
                "Feature persistence failed; runtime state is unchanged"
            }
            Self::Provider(FeatureControlProviderError::PersistenceDiverged) => {
                "Feature persistence rollback failed; runtime state was not committed"
            }
        }
    }
}

fn execute_feature_control_request<P: FeatureControlProvider + ?Sized>(
    provider: &P,
    request: &ManagerRequest,
) -> Result<FeatureControlOutcome, FeatureActionError> {
    let action = FeatureAction::parse(&request.action).ok_or(FeatureActionError::UnknownAction)?;
    let allowed = match action {
        FeatureAction::Dnd => &["devicename", "dndstate"][..],
        FeatureAction::Forwarding => {
            &["devicename", "linename", "forwardtype", "disable", "number"][..]
        }
    };
    let fields = parse_fields(request, allowed)?;
    let device_id = DeviceId::new(required(&fields, "devicename")?)
        .map_err(|_| FeatureActionError::InvalidSelector)?;
    let mutation = match action {
        FeatureAction::Dnd => FeatureControlMutation::Dnd {
            device_id,
            mode: parse_dnd(required(&fields, "dndstate")?)?,
        },
        FeatureAction::Forwarding => {
            let line = required(&fields, "linename")?;
            validate_selector(line)?;
            let kind = ForwardingKind::parse(required(&fields, "forwardtype")?)
                .ok_or(FeatureActionError::InvalidForwardingKind)?;
            let disable = fields
                .get("disable")
                .map(|value| parse_bool(value))
                .transpose()?
                .unwrap_or(false);
            let number = fields.get("number").map(|value| value.trim());
            let destination = if disable {
                if number.is_some_and(|value| !value.is_empty()) {
                    return Err(FeatureActionError::ConflictingDestination);
                }
                None
            } else {
                Some(validate_destination(
                    number.ok_or(FeatureActionError::InvalidDestination)?,
                )?)
            };
            FeatureControlMutation::Forwarding {
                device_id,
                line: line.to_owned(),
                kind,
                destination,
            }
        }
    };
    provider.apply(mutation).map_err(Into::into)
}

fn parse_fields(
    request: &ManagerRequest,
    allowed: &[&str],
) -> Result<BTreeMap<String, String>, FeatureActionError> {
    RequestFields::new(request)
        .collect(allowed, &[])
        .map_err(Into::into)
}

fn required<'a>(
    fields: &'a BTreeMap<String, String>,
    name: &str,
) -> Result<&'a str, FeatureActionError> {
    fields
        .get(name)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(FeatureActionError::MissingField)
}

fn parse_dnd(value: &str) -> Result<DndMode, FeatureActionError> {
    if value.eq_ignore_ascii_case("off") {
        Ok(DndMode::Off)
    } else if value.eq_ignore_ascii_case("silent") {
        Ok(DndMode::Silent)
    } else if value.eq_ignore_ascii_case("reject") {
        Ok(DndMode::Reject)
    } else {
        Err(FeatureActionError::InvalidDndMode)
    }
}

fn parse_bool(value: &str) -> Result<bool, FeatureActionError> {
    if matches_ignore_ascii_case(value, &["yes", "true", "on", "1"]) {
        Ok(true)
    } else if matches_ignore_ascii_case(value, &["no", "false", "off", "0"]) {
        Ok(false)
    } else {
        Err(FeatureActionError::InvalidDisable)
    }
}

fn matches_ignore_ascii_case(value: &str, accepted: &[&str]) -> bool {
    accepted
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}

fn validate_selector(value: &str) -> Result<(), FeatureActionError> {
    if value.is_empty()
        || value.len() > MAX_LINE_SELECTOR_BYTES
        || value.chars().any(char::is_control)
    {
        Err(FeatureActionError::InvalidSelector)
    } else {
        Ok(())
    }
}

fn validate_destination(value: &str) -> Result<ForwardingDestination, FeatureActionError> {
    ForwardingDestination::new(value).map_err(|_| FeatureActionError::InvalidDestination)
}

fn success_response(outcome: FeatureControlOutcome) -> ManagerResponse {
    let fields = match outcome {
        FeatureControlOutcome::Dnd {
            device_id,
            mode,
            changed,
        } => vec![
            public("DeviceId", device_id.as_str()),
            public(
                "DNDState",
                match mode {
                    DndMode::Off => "off",
                    DndMode::Silent => "silent",
                    DndMode::Reject => "reject",
                },
            ),
            public("Changed", yes_no(changed)),
        ],
        FeatureControlOutcome::Forwarding {
            device_id,
            line,
            kind,
            enabled,
            changed,
        } => vec![
            public("DeviceId", device_id.as_str()),
            public("Line", line),
            public("Forwardtype", kind.as_str()),
            public("Enabled", yes_no(enabled)),
            public("Changed", yes_no(changed)),
        ],
    };
    let fields = fields
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|_| Vec::new());
    if fields.is_empty() {
        return ManagerResponse::error(FeatureActionError::InvalidOutput.response_message())
            .expect("fixed feature-control error is valid");
    }
    ManagerResponse::success("Feature state updated")
        .expect("fixed feature-control success is valid")
        .with_fields(fields)
}

fn public(name: &'static str, value: impl ToString) -> Result<ManagerField, ManagerError> {
    ManagerField::public(name, value.to_string())
}

const fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::ami::manager::{ManagerRequestField, ManagerResponseKind};

    use super::*;

    #[derive(Clone, Default)]
    struct FakeProvider {
        mutations: Arc<Mutex<Vec<FeatureControlMutation>>>,
        error: Arc<Mutex<Option<FeatureControlProviderError>>>,
    }

    impl FeatureControlProvider for FakeProvider {
        fn apply(
            &self,
            mutation: FeatureControlMutation,
        ) -> Result<FeatureControlOutcome, FeatureControlProviderError> {
            if let Some(error) = *self.error.lock().unwrap() {
                return Err(error);
            }
            self.mutations.lock().unwrap().push(mutation.clone());
            Ok(match mutation {
                FeatureControlMutation::Dnd { device_id, mode } => FeatureControlOutcome::Dnd {
                    device_id,
                    mode,
                    changed: true,
                },
                FeatureControlMutation::Forwarding {
                    device_id,
                    line,
                    kind,
                    destination,
                } => FeatureControlOutcome::Forwarding {
                    device_id,
                    line,
                    kind,
                    enabled: destination.is_some(),
                    changed: true,
                },
            })
        }
    }

    fn request(action: &str, fields: &[(&str, &str)]) -> ManagerRequest {
        let mut request_fields = vec![ManagerRequestField {
            name: "Action".into(),
            value: action.into(),
            sensitive: false,
        }];
        request_fields.extend(fields.iter().map(|(name, value)| ManagerRequestField {
            name: (*name).into(),
            value: (*value).into(),
            sensitive: false,
        }));
        ManagerRequest {
            action: action.into(),
            fields: request_fields,
        }
    }

    fn response_value<'a>(response: &'a ManagerResponse, name: &str) -> Option<&'a str> {
        response
            .fields()
            .iter()
            .find(|field| field.name() == name)
            .and_then(ManagerField::public_value)
    }

    #[test]
    fn accepted_dnd_modes_become_typed_mutations_and_responses() {
        for (value, mode) in [
            ("off", DndMode::Off),
            ("SILENT", DndMode::Silent),
            ("reject", DndMode::Reject),
        ] {
            let provider = FakeProvider::default();
            let response = handle_feature_control_request(
                &provider,
                request(
                    SET_DND_ACTION,
                    &[("Devicename", "SEP001122334455"), ("DNDState", value)],
                ),
            );
            assert_eq!(response.kind(), ManagerResponseKind::Success);
            assert_eq!(response_value(&response, "Changed"), Some("yes"));
            assert_eq!(
                provider.mutations.lock().unwrap().as_slice(),
                &[FeatureControlMutation::Dnd {
                    device_id: DeviceId::new("SEP001122334455").unwrap(),
                    mode,
                }]
            );
        }
    }

    #[test]
    fn forwarding_set_and_disable_are_typed_and_never_echo_destination() {
        let provider = FakeProvider::default();
        let set = handle_feature_control_request(
            &provider,
            request(
                SET_FORWARDING_ACTION,
                &[
                    ("Devicename", "SEP001122334455"),
                    ("Linename", "1001"),
                    ("Forwardtype", "NoAnswer"),
                    ("Number", "private-2000"),
                ],
            ),
        );
        assert_eq!(set.kind(), ManagerResponseKind::Success);
        assert_eq!(response_value(&set, "Enabled"), Some("yes"));
        assert!(response_value(&set, "Number").is_none());

        let disable = handle_feature_control_request(
            &provider,
            request(
                SET_FORWARDING_ACTION,
                &[
                    ("Devicename", "SEP001122334455"),
                    ("Linename", "1001"),
                    ("Forwardtype", "busy"),
                    ("Disable", "yes"),
                ],
            ),
        );
        assert_eq!(disable.kind(), ManagerResponseKind::Success);
        assert_eq!(response_value(&disable, "Enabled"), Some("no"));
        let mutations = provider.mutations.lock().unwrap();
        assert!(matches!(
            &mutations[0],
            FeatureControlMutation::Forwarding {
                kind: ForwardingKind::NoAnswer,
                destination: Some(destination),
                ..
            } if destination.as_str() == "private-2000"
        ));
        assert!(matches!(
            &mutations[1],
            FeatureControlMutation::Forwarding {
                kind: ForwardingKind::Busy,
                destination: None,
                ..
            }
        ));
        assert!(!format!("{:?}", mutations[0]).contains("private-2000"));
    }

    #[test]
    fn forwarding_selector_keeps_every_device_appearance_in_the_ui_scope() {
        let appearances = [("1001", 3), ("2002", 1), ("1001", 3), ("3003", 2)];
        assert_eq!(
            forwarding_ui_line_instances(Some("2002"), appearances),
            Some(vec![1, 2, 3])
        );
        assert_eq!(
            forwarding_ui_line_instances(Some("missing"), appearances),
            None
        );
        assert_eq!(
            forwarding_ui_line_instances(None, appearances),
            Some(vec![1, 2, 3])
        );
    }

    #[test]
    fn duplicate_unknown_sensitive_missing_and_malformed_fields_fail_closed() {
        let provider = FakeProvider::default();
        let cases = [
            request(
                SET_DND_ACTION,
                &[
                    ("Devicename", "SEP001122334455"),
                    ("DNDState", "off"),
                    ("dndstate", "reject"),
                ],
            ),
            request(
                SET_DND_ACTION,
                &[
                    ("Devicename", "SEP001122334455"),
                    ("DNDState", "off"),
                    ("Secret", "private"),
                ],
            ),
            request(SET_DND_ACTION, &[("Devicename", "SEP001122334455")]),
            request(
                SET_DND_ACTION,
                &[("Devicename", "bad\nname"), ("DNDState", "off")],
            ),
            request(
                SET_DND_ACTION,
                &[
                    ("Devicename", "SEP001122334455"),
                    ("DNDState", "invented-private-mode"),
                ],
            ),
        ];
        for request in cases {
            let response = handle_feature_control_request(&provider, request);
            assert_eq!(response.kind(), ManagerResponseKind::Error);
            assert!(!response.message().unwrap().contains("private"));
        }
        let mut sensitive = request(
            SET_DND_ACTION,
            &[("Devicename", "SEP001122334455"), ("DNDState", "off")],
        );
        sensitive.fields[2].sensitive = true;
        assert_eq!(
            handle_feature_control_request(&provider, sensitive).kind(),
            ManagerResponseKind::Error
        );
        assert!(provider.mutations.lock().unwrap().is_empty());
    }

    #[test]
    fn forwarding_relationships_booleans_and_bounds_are_strict() {
        let provider = FakeProvider::default();
        let common = [
            ("Devicename", "SEP001122334455"),
            ("Linename", "1001"),
            ("Forwardtype", "all"),
        ];
        for extras in [
            vec![],
            vec![("Disable", "maybe")],
            vec![("Disable", "yes"), ("Number", "2000")],
            vec![("Number", "")],
            vec![("Forwardtype", "invented")],
        ] {
            let mut fields = common.to_vec();
            fields.extend(extras);
            assert_eq!(
                handle_feature_control_request(&provider, request(SET_FORWARDING_ACTION, &fields),)
                    .kind(),
                ManagerResponseKind::Error
            );
        }
        let oversized = "x".repeat(crate::call::forwarding::MAX_FORWARD_DESTINATION_BYTES + 1);
        let mut fields = common.to_vec();
        fields.push(("Number", oversized.as_str()));
        assert_eq!(
            handle_feature_control_request(&provider, request(SET_FORWARDING_ACTION, &fields),)
                .kind(),
            ManagerResponseKind::Error
        );
        let oversized_line = "1".repeat(MAX_LINE_SELECTOR_BYTES + 1);
        let fields = [
            ("Devicename", "SEP001122334455"),
            ("Linename", oversized_line.as_str()),
            ("Forwardtype", "all"),
            ("Number", "2000"),
        ];
        assert_eq!(
            handle_feature_control_request(&provider, request(SET_FORWARDING_ACTION, &fields),)
                .kind(),
            ManagerResponseKind::Error
        );
        assert!(provider.mutations.lock().unwrap().is_empty());
    }

    #[test]
    fn every_provider_failure_is_fixed_and_secret_safe() {
        for error in [
            FeatureControlProviderError::Unavailable,
            FeatureControlProviderError::DeviceNotFound,
            FeatureControlProviderError::LineNotFound,
            FeatureControlProviderError::FeatureDisabled,
            FeatureControlProviderError::Persistence,
            FeatureControlProviderError::PersistenceDiverged,
        ] {
            let provider = FakeProvider::default();
            *provider.error.lock().unwrap() = Some(error);
            let response = handle_feature_control_request(
                &provider,
                request(
                    SET_DND_ACTION,
                    &[("Devicename", "SEP001122334455"), ("DNDState", "reject")],
                ),
            );
            assert_eq!(response.kind(), ManagerResponseKind::Error);
            assert!(!response.message().unwrap().contains("SEP001122334455"));
        }
    }

    #[test]
    fn cli_forwarding_parser_uses_the_same_bounded_typed_mutation() {
        let enabled =
            parse_cli_forwarding_mutation("SEP001122334455", "1001", "noanswer", "private-2000")
                .unwrap();
        assert!(matches!(
            &enabled,
            FeatureControlMutation::Forwarding {
                kind: ForwardingKind::NoAnswer,
                destination: Some(destination),
                ..
            } if destination.as_str() == "private-2000"
        ));
        assert!(!format!("{enabled:?}").contains("private-2000"));
        assert!(matches!(
            parse_cli_forwarding_mutation("SEP001122334455", "1001", "busy", "off"),
            Ok(FeatureControlMutation::Forwarding {
                kind: ForwardingKind::Busy,
                destination: None,
                ..
            })
        ));
        assert_eq!(
            parse_cli_forwarding_mutation(
                "SEP001122334455",
                "1001",
                "all",
                &"9".repeat(crate::call::forwarding::MAX_FORWARD_DESTINATION_BYTES + 1),
            ),
            Err(FeatureCliError::InvalidDestination)
        );
        assert_eq!(
            parse_cli_forwarding_mutation("SEP001122334455", "1001\n", "all", "private"),
            Err(FeatureCliError::InvalidLine)
        );
        assert_eq!(
            parse_cli_forwarding_mutation("invalid device", "1001", "all", "private"),
            Err(FeatureCliError::InvalidDevice)
        );
        assert_eq!(
            parse_cli_forwarding_mutation(
                "SEP001122334455",
                &"1".repeat(MAX_LINE_SELECTOR_BYTES + 1),
                "all",
                "private",
            ),
            Err(FeatureCliError::InvalidLine)
        );
        assert_eq!(
            parse_cli_forwarding_mutation("SEP001122334455", "1001", "invented", "private"),
            Err(FeatureCliError::InvalidKind)
        );
    }

    #[test]
    fn cli_dnd_parser_uses_the_same_bounded_typed_mutation() {
        for (mode, expected) in [
            ("off", DndMode::Off),
            ("SILENT", DndMode::Silent),
            ("reject", DndMode::Reject),
        ] {
            assert_eq!(
                parse_cli_dnd_mutation("sep001122334455", mode),
                Ok(FeatureControlMutation::Dnd {
                    device_id: DeviceId::new("SEP001122334455").unwrap(),
                    mode: expected,
                })
            );
        }
        assert_eq!(
            parse_cli_dnd_mutation("bad-device", "off"),
            Err(FeatureCliError::InvalidDevice)
        );
        assert_eq!(
            parse_cli_dnd_mutation(" SEP001122334455", "off"),
            Err(FeatureCliError::InvalidDevice)
        );
        assert_eq!(
            parse_cli_dnd_mutation("SEP001122334455", "private-mode"),
            Err(FeatureCliError::InvalidDndMode)
        );

        let provider = FakeProvider::default();
        assert!(matches!(
            execute_cli_dnd(&provider, "SEP001122334455", "silent"),
            Ok(FeatureControlOutcome::Dnd {
                mode: DndMode::Silent,
                changed: true,
                ..
            })
        ));
        assert!(matches!(
            provider.mutations.lock().unwrap().as_slice(),
            [FeatureControlMutation::Dnd {
                mode: DndMode::Silent,
                ..
            }]
        ));

        *provider.error.lock().unwrap() = Some(FeatureControlProviderError::Persistence);
        assert_eq!(
            execute_cli_dnd(&provider, "SEP001122334455", "off"),
            Err(FeatureCliError::Provider(
                FeatureControlProviderError::Persistence
            ))
        );
    }

    #[test]
    fn action_names_are_unique_and_raii_registration_is_unavailable_in_development() {
        assert_ne!(SET_DND_ACTION, SET_FORWARDING_ACTION);
        for action in FeatureAction::ALL {
            let synopsis = action.synopsis();
            assert!(!synopsis.is_empty());
            assert!(
                synopsis.len() <= 30,
                "{} synopsis is too long",
                action.name()
            );
            assert!(
                synopsis.is_ascii() && synopsis.bytes().all(|byte| !byte.is_ascii_control()),
                "{} synopsis is not printable ASCII",
                action.name()
            );
        }
        assert!(matches!(
            register_feature_control_actions(
                FakeProvider::default(),
                crate::ami::manager::UnavailableManager,
            ),
            Err(ManagerError::Unavailable)
        ));
        let manager = crate::ami::manager::RollbackManager::fail_on(1);
        assert!(matches!(
            register_feature_control_actions(FakeProvider::default(), manager.clone()),
            Err(ManagerError::RegistrationFailed)
        ));
        manager.assert_partial_rollback(2, 1);
    }
}
