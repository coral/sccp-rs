//! Call-scoped terminal indications for SCCP-originated dialplan calls.
//!
//! `SCCPIndicate(busy|congestion|unavailable|invalid-number)` presents a
//! terminal state on the exact handset appearance owned by the current
//! channel. SCCP has no wire-level unavailable state, so `unavailable` uses
//! the standard congestion/reorder state with an accurate prompt override.

use thiserror::Error;

use crate::pbx::dialplan::{
    DialplanApplicationResult, DialplanBackend, DialplanCallbackError, DialplanError,
    DialplanLimits,
};
use crate::pbx::party::AsteriskChannel;

pub const CALL_INDICATION_APPLICATION: &str = "SCCPIndicate";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HandsetCallIndication {
    Busy,
    Congestion,
    Unavailable,
    InvalidNumber,
}

impl HandsetCallIndication {
    pub fn parse(arguments: &str) -> Result<Self, HandsetCallIndicationError> {
        if arguments.len() > 32 || arguments.chars().any(char::is_control) {
            return Err(HandsetCallIndicationError::InvalidArguments);
        }
        match arguments.trim().to_ascii_lowercase().as_str() {
            "busy" => Ok(Self::Busy),
            "congestion" => Ok(Self::Congestion),
            "unavailable" => Ok(Self::Unavailable),
            "invalid-number" | "invalid_number" => Ok(Self::InvalidNumber),
            _ => Err(HandsetCallIndicationError::InvalidArguments),
        }
    }
}

pub trait HandsetCallIndicationProvider: Send + Sync + 'static {
    fn apply(
        &self,
        channel: &AsteriskChannel<'_>,
        indication: HandsetCallIndication,
    ) -> Result<(), HandsetCallIndicationProviderError>;
}

pub struct HandsetCallIndicationApplication<P> {
    provider: P,
}

impl<P: HandsetCallIndicationProvider> HandsetCallIndicationApplication<P> {
    pub const fn new(provider: P) -> Self {
        Self { provider }
    }

    pub fn execute(
        &self,
        arguments: &str,
        channel: &AsteriskChannel<'_>,
    ) -> Result<(), HandsetCallIndicationError> {
        let indication = HandsetCallIndication::parse(arguments)?;
        self.provider.apply(channel, indication)?;
        Ok(())
    }
}

pub fn register_handset_call_indication_application<
    P: HandsetCallIndicationProvider,
    B: DialplanBackend,
>(
    provider: P,
    backend: B,
) -> Result<B::Registration, DialplanError> {
    let application = HandsetCallIndicationApplication::new(provider);
    backend.register_application(
        CALL_INDICATION_APPLICATION,
        "Present a terminal state on an SCCP handset",
        "Present busy, congestion, unavailable, or invalid-number on the current SCCP call",
        DialplanLimits {
            max_arguments_bytes: 32,
            max_value_bytes: 1,
            max_output_bytes: 1,
        },
        move |invocation| {
            application
                .execute(&invocation.arguments, &invocation.channel)
                .map(|()| DialplanApplicationResult::CONTINUE)
                .map_err(|_| DialplanCallbackError::Failed)
        },
    )
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum HandsetCallIndicationProviderError {
    #[error("the callback channel is not owned by this driver")]
    NotDriverChannel,
    #[error("the channel or handset appearance is unavailable")]
    Unavailable,
    #[error("the handset is not registered")]
    NotRegistered,
    #[error("the terminal indication could not be queued to the handset")]
    HandsetRejected,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum HandsetCallIndicationError {
    #[error("SCCPIndicate expects busy, congestion, unavailable, or invalid-number")]
    InvalidArguments,
    #[error(transparent)]
    Provider(#[from] HandsetCallIndicationProviderError),
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct FakeProvider {
        operations: Mutex<Vec<(usize, HandsetCallIndication)>>,
        failure: Option<HandsetCallIndicationProviderError>,
    }

    impl HandsetCallIndicationProvider for FakeProvider {
        fn apply(
            &self,
            channel: &AsteriskChannel<'_>,
            indication: HandsetCallIndication,
        ) -> Result<(), HandsetCallIndicationProviderError> {
            if let Some(error) = self.failure {
                return Err(error);
            }
            self.operations
                .lock()
                .unwrap()
                .push((channel.as_raw() as usize, indication));
            Ok(())
        }
    }

    fn channel() -> AsteriskChannel<'static> {
        let pointer = Box::leak(Box::new(1_u8));
        unsafe { AsteriskChannel::from_raw(std::ptr::from_mut(pointer).cast()).unwrap() }
    }

    #[test]
    fn parses_every_supported_terminal_indication() {
        for (arguments, expected) in [
            ("busy", HandsetCallIndication::Busy),
            ("CONGESTION", HandsetCallIndication::Congestion),
            (" unavailable ", HandsetCallIndication::Unavailable),
            ("invalid-number", HandsetCallIndication::InvalidNumber),
            ("invalid_number", HandsetCallIndication::InvalidNumber),
        ] {
            assert_eq!(HandsetCallIndication::parse(arguments), Ok(expected));
        }
    }

    #[test]
    fn rejects_empty_unknown_control_and_oversized_arguments() {
        for arguments in ["", "offline", "busy\n"] {
            assert_eq!(
                HandsetCallIndication::parse(arguments),
                Err(HandsetCallIndicationError::InvalidArguments)
            );
        }
        assert_eq!(
            HandsetCallIndication::parse(&"x".repeat(33)),
            Err(HandsetCallIndicationError::InvalidArguments)
        );
    }

    #[test]
    fn provider_receives_the_exact_channel_and_indication() {
        let application = HandsetCallIndicationApplication::new(FakeProvider {
            operations: Mutex::new(Vec::new()),
            failure: None,
        });
        let channel = channel();
        application.execute("unavailable", &channel).unwrap();
        assert_eq!(
            application.provider.operations.lock().unwrap().as_slice(),
            [(
                channel.as_raw() as usize,
                HandsetCallIndication::Unavailable
            )]
        );
    }

    #[test]
    fn provider_failures_remain_typed() {
        let application = HandsetCallIndicationApplication::new(FakeProvider {
            operations: Mutex::new(Vec::new()),
            failure: Some(HandsetCallIndicationProviderError::HandsetRejected),
        });
        assert_eq!(
            application.execute("unavailable", &channel()),
            Err(HandsetCallIndicationError::Provider(
                HandsetCallIndicationProviderError::HandsetRejected
            ))
        );
    }

    #[test]
    fn registration_is_unavailable_without_native_linkage() {
        let result = register_handset_call_indication_application(
            FakeProvider {
                operations: Mutex::new(Vec::new()),
                failure: None,
            },
            crate::pbx::dialplan::UnavailableDialplan,
        );
        assert!(matches!(result, Err(DialplanError::Unavailable)));
    }
}
