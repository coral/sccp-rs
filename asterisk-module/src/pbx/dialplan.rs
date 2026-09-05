//! Policy-free registration for bounded dialplan functions and applications.
//!
//! Function/application names and descriptions reject empty, control, NUL, and
//! unsafe text. [`DialplanLimits`] bound input arguments, assigned values and
//! output on both sides of the native callback. Requests copy the name and text
//! into owned Rust values while an [`AsteriskChannel`] guard holds the exact
//! channel reference.
//!
//! Duplicate names fail deterministically. Concrete backends own registration
//! teardown and callback-drain behavior.

use thiserror::Error;

use crate::pbx::party::AsteriskChannel;

const FLAG_HAS_READ: u32 = 1 << 0;
const FLAG_HAS_WRITE: u32 = 1 << 1;
const FLAG_ESCALATE_READ: u32 = 1 << 2;
const FLAG_ESCALATE_WRITE: u32 = 1 << 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DialplanLimits {
    pub max_arguments_bytes: usize,
    pub max_value_bytes: usize,
    pub max_output_bytes: usize,
}

impl Default for DialplanLimits {
    fn default() -> Self {
        Self {
            max_arguments_bytes: 4096,
            max_value_bytes: 4096,
            max_output_bytes: 4096,
        }
    }
}

impl DialplanLimits {
    pub fn validate(self) -> Result<Self, DialplanError> {
        if self.max_arguments_bytes == 0
            || self.max_value_bytes == 0
            || self.max_output_bytes == 0
            || self.max_arguments_bytes == usize::MAX
            || self.max_value_bytes == usize::MAX
            || self.max_output_bytes == usize::MAX
        {
            return Err(DialplanError::InvalidLimits);
        }
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DialplanEscalation {
    #[default]
    None,
    Read,
    Write,
    Both,
}

impl DialplanEscalation {
    const fn flags(self) -> u32 {
        match self {
            Self::None => 0,
            Self::Read => FLAG_ESCALATE_READ,
            Self::Write => FLAG_ESCALATE_WRITE,
            Self::Both => FLAG_ESCALATE_READ | FLAG_ESCALATE_WRITE,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DialplanApplicationResult(i32);

impl DialplanApplicationResult {
    pub const CONTINUE: Self = Self(0);
    pub const HANGUP: Self = Self(-1);

    pub const fn from_raw(value: i32) -> Self {
        Self(value)
    }

    pub const fn raw(self) -> i32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum DialplanCallbackError {
    #[error("dialplan callback failed")]
    Failed,
}

pub type DialplanCallbackResult<T> = Result<T, DialplanCallbackError>;

pub struct DialplanFunctionRead<'a> {
    pub channel: Option<AsteriskChannel<'a>>,
    pub name: String,
    pub arguments: String,
}

pub struct DialplanFunctionWrite<'a> {
    pub channel: Option<AsteriskChannel<'a>>,
    pub name: String,
    pub arguments: String,
    pub value: String,
}

pub struct DialplanApplicationInvocation<'a> {
    pub channel: AsteriskChannel<'a>,
    pub name: String,
    pub arguments: String,
}

pub(crate) type ReadHandler =
    dyn for<'a> Fn(DialplanFunctionRead<'a>) -> DialplanCallbackResult<String> + Send + Sync;
pub(crate) type WriteHandler =
    dyn for<'a> Fn(DialplanFunctionWrite<'a>) -> DialplanCallbackResult<()> + Send + Sync;
#[cfg(any(feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) type ApplicationHandler = dyn for<'a> Fn(
        DialplanApplicationInvocation<'a>,
    ) -> DialplanCallbackResult<DialplanApplicationResult>
    + Send
    + Sync;

#[derive(Default)]
pub struct DialplanFunctionHandlers {
    read: Option<Box<ReadHandler>>,
    write: Option<Box<WriteHandler>>,
}

impl DialplanFunctionHandlers {
    pub const fn new() -> Self {
        Self {
            read: None,
            write: None,
        }
    }

    pub fn with_read<F>(mut self, handler: F) -> Self
    where
        F: for<'a> Fn(DialplanFunctionRead<'a>) -> DialplanCallbackResult<String>
            + Send
            + Sync
            + 'static,
    {
        self.read = Some(Box::new(handler));
        self
    }

    pub fn with_write<F>(mut self, handler: F) -> Self
    where
        F: for<'a> Fn(DialplanFunctionWrite<'a>) -> DialplanCallbackResult<()>
            + Send
            + Sync
            + 'static,
    {
        self.write = Some(Box::new(handler));
        self
    }

    pub fn validate_for(&self, escalation: DialplanEscalation) -> Result<(), DialplanError> {
        self.flags(escalation).map(|_| ())
    }

    fn flags(&self, escalation: DialplanEscalation) -> Result<u32, DialplanError> {
        let mut flags = escalation.flags();
        if self.read.is_some() {
            flags |= FLAG_HAS_READ;
        }
        if self.write.is_some() {
            flags |= FLAG_HAS_WRITE;
        }
        if flags & (FLAG_HAS_READ | FLAG_HAS_WRITE) == 0 {
            return Err(DialplanError::MissingFunctionHandler);
        }
        if flags & FLAG_ESCALATE_READ != 0 && flags & FLAG_HAS_READ == 0
            || flags & FLAG_ESCALATE_WRITE != 0 && flags & FLAG_HAS_WRITE == 0
        {
            return Err(DialplanError::InvalidEscalation);
        }
        Ok(flags)
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) const fn has_read(&self) -> bool {
        self.read.is_some()
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) const fn has_write(&self) -> bool {
        self.write.is_some()
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn read(&self) -> Option<&ReadHandler> {
        self.read.as_deref()
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn write(&self) -> Option<&WriteHandler> {
        self.write.as_deref()
    }
}

/// Backend port for custom function and application registration.
pub trait DialplanBackend: Clone + Send + Sync + 'static {
    type Registration: Send + 'static;

    fn register_function(
        &self,
        name: &str,
        synopsis: &str,
        description: &str,
        escalation: DialplanEscalation,
        limits: DialplanLimits,
        handlers: DialplanFunctionHandlers,
    ) -> Result<Self::Registration, DialplanError>;

    fn register_application<F>(
        &self,
        name: &str,
        synopsis: &str,
        description: &str,
        limits: DialplanLimits,
        handler: F,
    ) -> Result<Self::Registration, DialplanError>
    where
        F: for<'a> Fn(
                DialplanApplicationInvocation<'a>,
            ) -> DialplanCallbackResult<DialplanApplicationResult>
            + Send
            + Sync
            + 'static;
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct UnavailableDialplan;

#[cfg(test)]
impl DialplanBackend for UnavailableDialplan {
    type Registration = ();

    fn register_function(
        &self,
        _name: &str,
        _synopsis: &str,
        _description: &str,
        _escalation: DialplanEscalation,
        _limits: DialplanLimits,
        _handlers: DialplanFunctionHandlers,
    ) -> Result<Self::Registration, DialplanError> {
        Err(DialplanError::Unavailable)
    }

    fn register_application<F>(
        &self,
        _name: &str,
        _synopsis: &str,
        _description: &str,
        _limits: DialplanLimits,
        _handler: F,
    ) -> Result<Self::Registration, DialplanError>
    where
        F: for<'a> Fn(
                DialplanApplicationInvocation<'a>,
            ) -> DialplanCallbackResult<DialplanApplicationResult>
            + Send
            + Sync
            + 'static,
    {
        Err(DialplanError::Unavailable)
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum DialplanError {
    #[error(
        "dialplan name must begin with a letter and contain only ASCII letters, digits, or underscore"
    )]
    InvalidName,
    #[error("dialplan synopsis must be 1..=64 printable ASCII bytes")]
    InvalidSynopsis,
    #[error("dialplan description must be non-empty text without a NUL or carriage return")]
    InvalidDescription,
    #[error("dialplan limits must be non-zero and leave space for a terminator")]
    InvalidLimits,
    #[error("a custom function requires at least one read or write handler")]
    MissingFunctionHandler,
    #[error("read/write privilege escalation requires the corresponding handler")]
    InvalidEscalation,
    #[error("unable to register dialplan entry")]
    RegistrationFailed,
    #[error("Asterisk dialplan registration is unavailable in development builds")]
    Unavailable,
}

pub fn validated_name(value: &str) -> Result<String, DialplanError> {
    if value.is_empty()
        || value.len() > 64
        || !value.is_ascii()
        || !value.as_bytes()[0].is_ascii_alphabetic()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(DialplanError::InvalidName);
    }
    Ok(value.to_owned())
}

pub fn validated_synopsis(value: &str) -> Result<String, DialplanError> {
    if value.is_empty()
        || value.len() > 64
        || !value.is_ascii()
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(DialplanError::InvalidSynopsis);
    }
    Ok(value.to_owned())
}

pub fn validated_description(value: &str) -> Result<String, DialplanError> {
    if value.is_empty() || value.contains(['\0', '\r']) {
        return Err(DialplanError::InvalidDescription);
    }
    Ok(value.to_owned())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn typed_function_handlers_receive_owned_requests() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let captured_writes = Arc::clone(&writes);
        let handlers = DialplanFunctionHandlers::new()
            .with_read(|request| {
                assert!(request.channel.is_none());
                assert_eq!(request.name, "SCCP_STATE");
                assert_eq!(request.arguments, "SEP001,status");
                Ok("registered".to_owned())
            })
            .with_write(move |request| {
                captured_writes.lock().unwrap().push((
                    request.name,
                    request.arguments,
                    request.value,
                    request.channel.is_some(),
                ));
                Ok(())
            });

        let read = handlers.read().expect("read handler");
        assert_eq!(
            read(DialplanFunctionRead {
                channel: None,
                name: "SCCP_STATE".to_owned(),
                arguments: "SEP001,status".to_owned(),
            })
            .unwrap(),
            "registered"
        );

        let write = handlers.write().expect("write handler");
        write(DialplanFunctionWrite {
            channel: None,
            name: "SCCP_STATE".to_owned(),
            arguments: "SEP001,dnd".to_owned(),
            value: "reject".to_owned(),
        })
        .unwrap();
        assert_eq!(
            *writes.lock().unwrap(),
            [(
                "SCCP_STATE".into(),
                "SEP001,dnd".into(),
                "reject".into(),
                false
            )]
        );
    }

    #[test]
    fn validates_names_docs_handlers_escalation_and_limits() {
        for invalid in ["", "1FUNC", "BAD-NAME", "BAD NAME", "ümlaut"] {
            assert!(matches!(
                validated_name(invalid),
                Err(DialplanError::InvalidName)
            ));
        }
        assert!(validated_name("SCCP_STATE1").is_ok());
        assert!(matches!(
            validated_synopsis("bad\nsummary"),
            Err(DialplanError::InvalidSynopsis)
        ));
        assert!(matches!(
            validated_description("bad\rdescription"),
            Err(DialplanError::InvalidDescription)
        ));
        assert!(matches!(
            DialplanFunctionHandlers::new().flags(DialplanEscalation::None),
            Err(DialplanError::MissingFunctionHandler)
        ));
        assert!(matches!(
            DialplanFunctionHandlers::new()
                .with_read(|_| Ok(String::new()))
                .flags(DialplanEscalation::Write),
            Err(DialplanError::InvalidEscalation)
        ));
        assert!(matches!(
            DialplanLimits {
                max_output_bytes: 0,
                ..DialplanLimits::default()
            }
            .validate(),
            Err(DialplanError::InvalidLimits)
        ));
        assert!(matches!(
            DialplanLimits {
                max_arguments_bytes: usize::MAX,
                ..DialplanLimits::default()
            }
            .validate(),
            Err(DialplanError::InvalidLimits)
        ));
    }

    #[test]
    fn handler_shape_and_escalation_are_kept_separate() {
        let read = DialplanFunctionHandlers::new().with_read(|_| Ok(String::new()));
        assert!(read.has_read());
        assert!(!read.has_write());
        assert_eq!(
            read.flags(DialplanEscalation::Read).unwrap(),
            FLAG_HAS_READ | FLAG_ESCALATE_READ
        );

        let write = DialplanFunctionHandlers::new().with_write(|_| Ok(()));
        assert!(!write.has_read());
        assert!(write.has_write());
        assert_eq!(
            write.flags(DialplanEscalation::Write).unwrap(),
            FLAG_HAS_WRITE | FLAG_ESCALATE_WRITE
        );
    }

    #[test]
    fn public_api_is_explicitly_unavailable_without_native_linkage() {
        let result = UnavailableDialplan.register_function(
            "SCCP_STATE",
            "Read endpoint state",
            "Read endpoint state",
            DialplanEscalation::None,
            DialplanLimits::default(),
            DialplanFunctionHandlers::new().with_read(|_| Ok(String::new())),
        );
        assert!(matches!(result, Err(DialplanError::Unavailable)));
        let result = UnavailableDialplan.register_application(
            "SCCPAction",
            "Run endpoint action",
            "Run endpoint action",
            DialplanLimits::default(),
            |_| Ok(DialplanApplicationResult::CONTINUE),
        );
        assert!(matches!(result, Err(DialplanError::Unavailable)));
    }
}
