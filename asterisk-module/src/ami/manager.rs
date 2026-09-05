//! Policy-free, bounded Asterisk Manager Interface actions and events.
//!
//! This module owns the public AMI value model and validation policy. The
//! Asterisk adapter stores typed handlers and converts raw manager messages at
//! the actual foreign callback edge; no project-owned C records or callback
//! userdata are visible here.

use std::collections::BTreeMap;

use thiserror::Error;

const ALL_PRIVILEGES: u32 = (1 << 0)
    | (1 << 1)
    | (1 << 2)
    | (1 << 3)
    | (1 << 4)
    | (1 << 5)
    | (1 << 6)
    | (1 << 7)
    | (1 << 8)
    | (1 << 9)
    | (1 << 10)
    | (1 << 11)
    | (1 << 12)
    | (1 << 13)
    | (1 << 14)
    | (1 << 15)
    | (1 << 16)
    | (1 << 17)
    | (1 << 18)
    | (1 << 30);

/// A permission mask accepted by Asterisk's manager API.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ManagerPrivilege(u32);

impl ManagerPrivilege {
    pub const NONE: Self = Self(0);
    pub const SYSTEM: Self = Self(1 << 0);
    pub const CALL: Self = Self(1 << 1);
    pub const LOG: Self = Self(1 << 2);
    pub const VERBOSE: Self = Self(1 << 3);
    pub const COMMAND: Self = Self(1 << 4);
    pub const AGENT: Self = Self(1 << 5);
    pub const USER: Self = Self(1 << 6);
    pub const CONFIG: Self = Self(1 << 7);
    pub const DTMF: Self = Self(1 << 8);
    pub const REPORTING: Self = Self(1 << 9);
    pub const CDR: Self = Self(1 << 10);
    pub const DIALPLAN: Self = Self(1 << 11);
    pub const ORIGINATE: Self = Self(1 << 12);
    pub const AGI: Self = Self(1 << 13);
    pub const HOOK_RESPONSE: Self = Self(1 << 14);
    pub const CALL_COMPLETION: Self = Self(1 << 15);
    pub const ADVICE_OF_CHARGE: Self = Self(1 << 16);
    pub const TEST: Self = Self(1 << 17);
    pub const SECURITY: Self = Self(1 << 18);
    pub const MESSAGE: Self = Self(1 << 30);

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub(crate) const fn is_valid(self) -> bool {
        self.0 & !ALL_PRIVILEGES == 0
    }
}

/// Explicit limits applied while decoding requests and encoding output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ManagerLimits {
    pub max_fields: usize,
    pub max_field_name_bytes: usize,
    pub max_field_value_bytes: usize,
    pub max_response_bytes: usize,
}

impl Default for ManagerLimits {
    fn default() -> Self {
        Self {
            max_fields: 128,
            max_field_name_bytes: 64,
            max_field_value_bytes: 4096,
            max_response_bytes: 64 * 1024,
        }
    }
}

impl ManagerLimits {
    pub fn validate(self) -> Result<Self, ManagerError> {
        if self.max_fields == 0
            || self.max_field_name_bytes == 0
            || self.max_field_value_bytes == 0
            || self.max_response_bytes == 0
        {
            return Err(ManagerError::ZeroLimit);
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ManagerFieldValue {
    Public(String),
    Redacted,
}

/// An outbound response or event field.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagerField {
    name: String,
    value: ManagerFieldValue,
}

impl ManagerField {
    pub fn public(name: impl Into<String>, value: impl Into<String>) -> Result<Self, ManagerError> {
        let name = name.into();
        let value = value.into();
        validate_field_name(&name)?;
        validate_field_value(&value)?;
        Ok(Self {
            name,
            value: ManagerFieldValue::Public(value),
        })
    }

    /// Emit the field with the fixed value `<redacted>`.
    pub fn redacted(name: impl Into<String>) -> Result<Self, ManagerError> {
        let name = name.into();
        validate_field_name(&name)?;
        Ok(Self {
            name,
            value: ManagerFieldValue::Redacted,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn public_value(&self) -> Option<&str> {
        match &self.value {
            ManagerFieldValue::Public(value) => Some(value),
            ManagerFieldValue::Redacted => None,
        }
    }

    pub const fn is_redacted(&self) -> bool {
        matches!(self.value, ManagerFieldValue::Redacted)
    }
}

/// One owned field from an incoming manager action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagerRequestField {
    pub name: String,
    pub value: String,
    /// True for names commonly used to carry credentials or tokens.
    pub sensitive: bool,
}

impl ManagerRequestField {
    /// Build an owned request field and classify credential-bearing names at
    /// the point where untrusted manager input becomes typed Rust data.
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            sensitive: request_field_name_sensitive(&name),
            name,
            value: value.into(),
        }
    }
}

/// An owned request. Field order and repeated names are preserved.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagerRequest {
    pub action: String,
    pub fields: Vec<ManagerRequestField>,
}

impl ManagerRequest {
    pub fn values<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.fields
            .iter()
            .filter(move |field| field.name.eq_ignore_ascii_case(name))
            .map(|field| field.value.as_str())
    }
}

/// Common validation result for action-specific request field policies.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestFieldsError {
    Sensitive,
    Duplicate,
    Unknown,
    ActionMismatch,
}

/// Implements the common manager-field validation translation while allowing
/// each action family to retain its public response wording.
macro_rules! impl_request_fields_error {
    ($error:ty) => {
        impl From<$crate::ami::manager::RequestFieldsError> for $error {
            fn from(error: $crate::ami::manager::RequestFieldsError) -> Self {
                match error {
                    $crate::ami::manager::RequestFieldsError::Sensitive => Self::SensitiveField,
                    $crate::ami::manager::RequestFieldsError::Duplicate => Self::DuplicateField,
                    $crate::ami::manager::RequestFieldsError::Unknown => Self::UnknownField,
                    $crate::ami::manager::RequestFieldsError::ActionMismatch => Self::UnknownAction,
                }
            }
        }
    };
}

pub(crate) use impl_request_fields_error;

/// A policy-aware view over one manager request.
///
/// Action handlers retain their own required-field and value policy while
/// sharing the security-sensitive mechanics: case folding, protocol-header
/// validation, duplicate detection, aliases, sensitive fields, and rejection
/// of unknown names.
pub struct RequestFields<'a> {
    request: &'a ManagerRequest,
}

impl<'a> RequestFields<'a> {
    pub const fn new(request: &'a ManagerRequest) -> Self {
        Self { request }
    }

    /// Collect allowed fields by their lowercase canonical name.
    ///
    /// `aliases` contains `(alias, canonical)` pairs. Both spellings occupy
    /// the same slot, so combining an alias with its canonical name is a
    /// duplicate. Protocol `Action` and `ActionID` headers are validated but
    /// are not returned.
    pub fn collect(
        &self,
        allowed: &[&str],
        aliases: &[(&str, &str)],
    ) -> Result<BTreeMap<String, String>, RequestFieldsError> {
        let mut fields = BTreeMap::new();
        let mut protocol_fields = [false; 2];
        for field in &self.request.fields {
            if field.sensitive {
                return Err(RequestFieldsError::Sensitive);
            }
            let name = field.name.to_ascii_lowercase();
            if let Some(index) = match name.as_str() {
                "action" => Some(0),
                "actionid" => Some(1),
                _ => None,
            } {
                if std::mem::replace(&mut protocol_fields[index], true) {
                    return Err(RequestFieldsError::Duplicate);
                }
                if index == 0 && !field.value.eq_ignore_ascii_case(&self.request.action) {
                    return Err(RequestFieldsError::ActionMismatch);
                }
                continue;
            }
            let canonical = aliases
                .iter()
                .find_map(|(alias, canonical)| (*alias == name).then_some(*canonical))
                .unwrap_or(name.as_str());
            if !allowed.contains(&canonical) {
                return Err(RequestFieldsError::Unknown);
            }
            if fields
                .insert(canonical.to_owned(), field.value.clone())
                .is_some()
            {
                return Err(RequestFieldsError::Duplicate);
            }
        }
        Ok(fields)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagerResponseKind {
    Success,
    Error,
}

/// An owned response returned by an action handler.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagerResponse {
    kind: ManagerResponseKind,
    message: Option<String>,
    fields: Vec<ManagerField>,
}

impl ManagerResponse {
    pub fn success(message: impl Into<String>) -> Result<Self, ManagerError> {
        Self::new(
            ManagerResponseKind::Success,
            Some(message.into()),
            Vec::new(),
        )
    }

    pub fn error(message: impl Into<String>) -> Result<Self, ManagerError> {
        Self::new(ManagerResponseKind::Error, Some(message.into()), Vec::new())
    }

    pub fn new(
        kind: ManagerResponseKind,
        message: Option<String>,
        fields: Vec<ManagerField>,
    ) -> Result<Self, ManagerError> {
        if let Some(message) = &message {
            validate_message(message)?;
        }
        Ok(Self {
            kind,
            message,
            fields,
        })
    }

    pub fn with_fields(mut self, fields: Vec<ManagerField>) -> Self {
        self.fields = fields;
        self
    }

    pub const fn kind(&self) -> ManagerResponseKind {
        self.kind
    }

    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    pub fn fields(&self) -> &[ManagerField] {
        &self.fields
    }
}

/// A bounded event ready for publication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagerEvent {
    category: ManagerPrivilege,
    name: String,
    fields: Vec<ManagerField>,
}

impl ManagerEvent {
    pub fn new(
        category: ManagerPrivilege,
        name: impl Into<String>,
        fields: Vec<ManagerField>,
    ) -> Result<Self, ManagerError> {
        if category == ManagerPrivilege::NONE || !category.is_valid() {
            return Err(ManagerError::InvalidPrivileges);
        }
        let name = name.into();
        validate_manager_token(&name, "event")?;
        Ok(Self {
            category,
            name,
            fields,
        })
    }

    pub const fn category(&self) -> ManagerPrivilege {
        self.category
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn fields(&self) -> &[ManagerField] {
        &self.fields
    }
}

#[cfg(any(feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) type ManagerActionHandler =
    dyn Fn(ManagerRequest) -> ManagerResponse + Send + Sync + 'static;

/// Backend port for AMI action registration and event publication.
pub trait ManagerBackend: Clone + Send + Sync + 'static {
    type Registration: Send + 'static;

    fn register_action<F>(
        &self,
        action: &str,
        authority: ManagerPrivilege,
        synopsis: &str,
        description: &str,
        limits: ManagerLimits,
        handler: F,
    ) -> Result<Self::Registration, ManagerError>
    where
        F: Fn(ManagerRequest) -> ManagerResponse + Send + Sync + 'static;

    fn publish(&self, event: &ManagerEvent, limits: ManagerLimits) -> Result<(), ManagerError>;
}

/// Complete registration record for one member of an AMI action group.
pub(crate) trait ActionDefinition<P>: Copy {
    fn name(self) -> &'static str;
    fn synopsis(self) -> &'static str;
    fn description(self) -> &'static str;
    fn privileges(self) -> ManagerPrivilege;
    fn limits(self) -> ManagerLimits;
    fn handle(self, provider: &P, request: ManagerRequest) -> ManagerResponse;

    fn handler(
        self,
        provider: std::sync::Arc<P>,
    ) -> Box<dyn Fn(ManagerRequest) -> ManagerResponse + Send + Sync + 'static>
    where
        P: Send + Sync + 'static,
        Self: Send + Sync + 'static,
    {
        Box::new(move |request| self.handle(provider.as_ref(), request))
    }
}

/// Registers a metadata-driven group with one shared provider allocation.
/// Dropping the partially built vector rolls back earlier registrations when
/// any later action fails.
pub(crate) fn register_action_group<P, M, A>(
    provider: P,
    manager: M,
    actions: &[A],
) -> Result<Vec<M::Registration>, ManagerError>
where
    P: Send + Sync + 'static,
    M: ManagerBackend,
    A: ActionDefinition<P> + Send + Sync + 'static,
{
    let provider = std::sync::Arc::new(provider);
    let mut registrations = Vec::with_capacity(actions.len());
    for &action in actions {
        let handler = action.handler(std::sync::Arc::clone(&provider));
        registrations.push(manager.register_action(
            action.name(),
            action.privileges(),
            action.synopsis(),
            action.description(),
            action.limits(),
            handler,
        )?);
    }
    Ok(registrations)
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct UnavailableManager;

#[cfg(test)]
impl ManagerBackend for UnavailableManager {
    type Registration = ();

    fn register_action<F>(
        &self,
        _action: &str,
        _authority: ManagerPrivilege,
        _synopsis: &str,
        _description: &str,
        _limits: ManagerLimits,
        _handler: F,
    ) -> Result<Self::Registration, ManagerError>
    where
        F: Fn(ManagerRequest) -> ManagerResponse + Send + Sync + 'static,
    {
        Err(ManagerError::Unavailable)
    }

    fn publish(&self, _event: &ManagerEvent, _limits: ManagerLimits) -> Result<(), ManagerError> {
        Err(ManagerError::Unavailable)
    }
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct RollbackManager {
    state: std::sync::Arc<RollbackState>,
    fail_on: usize,
}

#[cfg(test)]
#[derive(Default)]
struct RollbackState {
    attempts: std::sync::atomic::AtomicUsize,
    drops: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
pub(crate) struct RollbackRegistration {
    state: std::sync::Arc<RollbackState>,
}

#[cfg(test)]
impl Drop for RollbackRegistration {
    fn drop(&mut self) {
        self.state
            .drops
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
impl RollbackManager {
    pub(crate) fn fail_on(fail_on: usize) -> Self {
        Self {
            state: std::sync::Arc::new(RollbackState::default()),
            fail_on,
        }
    }

    pub(crate) fn assert_partial_rollback(&self, attempts: usize, drops: usize) {
        assert_eq!(
            self.state
                .attempts
                .load(std::sync::atomic::Ordering::SeqCst),
            attempts
        );
        assert_eq!(
            self.state.drops.load(std::sync::atomic::Ordering::SeqCst),
            drops
        );
    }
}

#[cfg(test)]
impl ManagerBackend for RollbackManager {
    type Registration = RollbackRegistration;

    fn register_action<F>(
        &self,
        _action: &str,
        _authority: ManagerPrivilege,
        _synopsis: &str,
        _description: &str,
        _limits: ManagerLimits,
        _handler: F,
    ) -> Result<Self::Registration, ManagerError>
    where
        F: Fn(ManagerRequest) -> ManagerResponse + Send + Sync + 'static,
    {
        let attempt = self
            .state
            .attempts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if attempt == self.fail_on {
            return Err(ManagerError::RegistrationFailed);
        }
        Ok(RollbackRegistration {
            state: std::sync::Arc::clone(&self.state),
        })
    }

    fn publish(&self, _event: &ManagerEvent, _limits: ManagerLimits) -> Result<(), ManagerError> {
        Ok(())
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ManagerError {
    #[error("manager action must be a non-empty token of at most 64 bytes")]
    InvalidAction,
    #[error("manager event must be a non-empty token of at most 64 bytes")]
    InvalidEvent,
    #[error("manager field name must be a non-empty token")]
    InvalidFieldName,
    #[error("manager field name {0:?} is reserved by the protocol")]
    ReservedFieldName(String),
    #[error("manager field value must not contain a NUL, carriage return, or newline")]
    InvalidFieldValue,
    #[error("manager response message must be non-empty and single-line")]
    InvalidMessage,
    #[error("manager synopsis must be 1..=30 printable ASCII bytes")]
    InvalidSynopsis,
    #[error("manager description must be non-empty text without a NUL or carriage return")]
    InvalidDescription,
    #[error("manager privilege mask contains unsupported bits")]
    InvalidPrivileges,
    #[error("manager bounds must all be greater than zero")]
    ZeroLimit,
    #[error("unable to register manager action")]
    RegistrationFailed,
    #[error("manager event publication failed")]
    PublishFailed,
    #[error("Asterisk manager support is unavailable in development builds")]
    Unavailable,
}

pub(crate) fn validate_manager_token(value: &str, kind: &'static str) -> Result<(), ManagerError> {
    if value.is_empty()
        || value.len() > 64
        || !value.is_ascii()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(if kind == "action" {
            ManagerError::InvalidAction
        } else {
            ManagerError::InvalidEvent
        });
    }
    Ok(())
}

fn validate_field_name(name: &str) -> Result<(), ManagerError> {
    if name.is_empty()
        || !name.is_ascii()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(ManagerError::InvalidFieldName);
    }
    if ["response", "actionid", "message", "event", "privilege"]
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
    {
        return Err(ManagerError::ReservedFieldName(name.to_owned()));
    }
    Ok(())
}

fn validate_field_value(value: &str) -> Result<(), ManagerError> {
    if value.contains(['\0', '\r', '\n']) {
        Err(ManagerError::InvalidFieldValue)
    } else {
        Ok(())
    }
}

pub(crate) fn request_field_name_sensitive(name: &str) -> bool {
    [
        "secret",
        "password",
        "authorization",
        "authtoken",
        "token",
        "key",
    ]
    .iter()
    .any(|sensitive| name.eq_ignore_ascii_case(sensitive))
}

fn validate_message(message: &str) -> Result<(), ManagerError> {
    if message.is_empty() || message.contains(['\0', '\r', '\n']) {
        Err(ManagerError::InvalidMessage)
    } else {
        Ok(())
    }
}

pub fn validate_synopsis(synopsis: &str) -> Result<(), ManagerError> {
    if synopsis.is_empty()
        || synopsis.len() > 30
        || !synopsis.is_ascii()
        || synopsis.bytes().any(|byte| byte.is_ascii_control())
    {
        Err(ManagerError::InvalidSynopsis)
    } else {
        Ok(())
    }
}

pub fn validate_description(description: &str) -> Result<(), ManagerError> {
    if description.is_empty() || description.contains(['\0', '\r']) {
        Err(ManagerError::InvalidDescription)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_lookup_preserves_repeated_fields() {
        let request = ManagerRequest {
            action: "SccpInspect".to_owned(),
            fields: vec![
                ManagerRequestField {
                    name: "Line".to_owned(),
                    value: "1001".to_owned(),
                    sensitive: false,
                },
                ManagerRequestField {
                    name: "line".to_owned(),
                    value: "1002".to_owned(),
                    sensitive: false,
                },
            ],
        };
        assert_eq!(request.values("LINE").collect::<Vec<_>>(), ["1001", "1002"]);
    }

    #[test]
    fn request_fields_apply_alias_duplicate_sensitive_and_unknown_policy() {
        let request = ManagerRequest {
            action: "SccpInspect".to_owned(),
            fields: vec![
                ManagerRequestField::new("Action", "sccpinspect"),
                ManagerRequestField::new("DeviceName", "SEP001122334455"),
            ],
        };
        let fields = RequestFields::new(&request)
            .collect(&["deviceid"], &[("devicename", "deviceid")])
            .unwrap();
        assert_eq!(
            fields.get("deviceid").map(String::as_str),
            Some("SEP001122334455")
        );

        let mut duplicate = request;
        duplicate
            .fields
            .push(ManagerRequestField::new("DeviceId", "SEP00AABBCCDDEE"));
        assert_eq!(
            RequestFields::new(&duplicate).collect(&["deviceid"], &[("devicename", "deviceid")]),
            Err(RequestFieldsError::Duplicate)
        );

        let sensitive = ManagerRequest {
            action: "SccpInspect".to_owned(),
            fields: vec![ManagerRequestField::new("Secret", "private")],
        };
        assert_eq!(
            RequestFields::new(&sensitive).collect(&["secret"], &[]),
            Err(RequestFieldsError::Sensitive)
        );

        let unknown = ManagerRequest {
            action: "SccpInspect".to_owned(),
            fields: vec![ManagerRequestField::new("Unexpected", "value")],
        };
        assert_eq!(
            RequestFields::new(&unknown).collect(&[], &[]),
            Err(RequestFieldsError::Unknown)
        );
    }

    #[test]
    fn validates_protocol_text_privileges_and_limits() {
        assert!(
            ManagerPrivilege::CALL
                .union(ManagerPrivilege::USER)
                .contains(ManagerPrivilege::CALL)
        );
        assert!(matches!(
            ManagerEvent::new(ManagerPrivilege::NONE, "State", Vec::new()),
            Err(ManagerError::InvalidPrivileges)
        ));
        assert!(matches!(
            ManagerEvent::new(ManagerPrivilege::CALL, "bad event", Vec::new()),
            Err(ManagerError::InvalidEvent)
        ));
        assert!(matches!(
            ManagerField::public("Event", "injected"),
            Err(ManagerError::ReservedFieldName(_))
        ));
        assert!(matches!(
            ManagerField::public("Safe", "one\r\nEvent: injected"),
            Err(ManagerError::InvalidFieldValue)
        ));
        assert!(matches!(
            ManagerResponse::success("one\nInjected: yes"),
            Err(ManagerError::InvalidMessage)
        ));
        assert!(matches!(
            ManagerLimits {
                max_fields: 0,
                ..ManagerLimits::default()
            }
            .validate(),
            Err(ManagerError::ZeroLimit)
        ));
        assert!(!ManagerPrivilege(1 << 29).is_valid());
    }
}
