//! Typed policy and backend port for Call Completion Supplementary Service.
//!
//! The domain owns handset transaction identity, presentation ownership,
//! callback-target normalization, and typed failures. A concrete adapter owns
//! Asterisk's CCSS generic agent/monitor APIs and channel pointers.

use thiserror::Error;

/// The Asterisk CCSS core transaction accepted by the handset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CallCompletionTicket {
    pub core_id: u32,
}

/// Failure from the generic-agent lifecycle.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CallCompletionError {
    #[error("Asterisk call completion is unavailable")]
    Unavailable,
    #[error("this channel has no current call-completion offer")]
    NoOffer,
    #[error("the system call-completion request limit was reached")]
    LimitReached,
    #[error("Asterisk rejected the call-completion request")]
    Rejected,
}

impl CallCompletionError {
    /// Handset feedback for the exact backend rejection, never a speculative
    /// success prompt.
    pub const fn handset_prompt(self) -> &'static str {
        match self {
            Self::NoOffer => "Callback is not available for this call",
            Self::LimitReached => "Too many callback requests",
            Self::Unavailable => "Callback service is unavailable",
            Self::Rejected => "Callback request failed",
        }
    }
}

/// Exact handset presentation that owns a Callback key press.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CallCompletionOwnership<'a> {
    pub requested_device: &'a str,
    pub requested_call_id: u64,
    pub owner_device: Option<&'a str>,
    pub owner_call_id: Option<u64>,
}

/// Typed port implemented by a concrete telephony backend.
pub trait CallCompletionBackend<Channel>: Send + Sync {
    fn configure(&self, channel: &Channel) -> Result<(), CallCompletionError>;

    fn request(&self, channel: &Channel) -> Result<CallCompletionTicket, CallCompletionError>;
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) fn request_owned_with<B, Channel>(
    backend: &B,
    ownership: CallCompletionOwnership<'_>,
    channel: Option<&Channel>,
) -> Result<CallCompletionTicket, CallCompletionError>
where
    B: CallCompletionBackend<Channel>,
{
    if ownership.owner_device != Some(ownership.requested_device)
        || ownership.owner_call_id != Some(ownership.requested_call_id)
    {
        return Err(CallCompletionError::Unavailable);
    }
    backend.request(channel.ok_or(CallCompletionError::Unavailable)?)
}

/// Canonical SCCP device-state/dialstring target for the channel-tech busy
/// callback. Shared appearances of one logical line collapse to one target;
/// different logical lines are ambiguous and are not offered to CCSS.
pub fn canonical_callback_target(
    lines: impl IntoIterator<Item = String>,
) -> Result<String, CallCompletionTargetError> {
    let mut lines = lines.into_iter().collect::<Vec<_>>();
    lines.sort();
    lines.dedup();
    let [line] = lines.as_slice() else {
        return Err(if lines.is_empty() {
            CallCompletionTargetError::Unavailable
        } else {
            CallCompletionTargetError::Ambiguous
        });
    };
    Ok(format!("SCCP/{line}"))
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CallCompletionTargetError {
    #[error("the SCCP callback target is unavailable")]
    Unavailable,
    #[error("the SCCP callback target resolves to multiple logical lines")]
    Ambiguous,
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;

    struct FakeBackend {
        configure: Result<(), CallCompletionError>,
        requests: Mutex<VecDeque<Result<CallCompletionTicket, CallCompletionError>>>,
        channels: Mutex<Vec<usize>>,
    }

    impl CallCompletionBackend<usize> for FakeBackend {
        fn configure(&self, channel: &usize) -> Result<(), CallCompletionError> {
            self.channels.lock().unwrap().push(*channel);
            self.configure
        }

        fn request(&self, channel: &usize) -> Result<CallCompletionTicket, CallCompletionError> {
            self.channels.lock().unwrap().push(*channel);
            self.requests.lock().unwrap().pop_front().unwrap()
        }
    }

    fn ownership<'a>(
        requested_device: &'a str,
        requested_call_id: u64,
        owner_device: Option<&'a str>,
        owner_call_id: Option<u64>,
    ) -> CallCompletionOwnership<'a> {
        CallCompletionOwnership {
            requested_device,
            requested_call_id,
            owner_device,
            owner_call_id,
        }
    }

    #[test]
    fn configures_and_accepts_the_exact_typed_core_transaction() {
        let backend = FakeBackend {
            configure: Ok(()),
            requests: Mutex::new(VecDeque::from([Ok(CallCompletionTicket { core_id: 73 })])),
            channels: Mutex::new(Vec::new()),
        };
        assert_eq!(backend.configure(&0x1234), Ok(()));
        assert_eq!(
            backend.request(&0x1234),
            Ok(CallCompletionTicket { core_id: 73 })
        );
        assert_eq!(*backend.channels.lock().unwrap(), vec![0x1234, 0x1234]);
    }

    #[test]
    fn backend_failures_are_typed_without_status_codes() {
        for expected in [
            CallCompletionError::Unavailable,
            CallCompletionError::NoOffer,
            CallCompletionError::LimitReached,
            CallCompletionError::Rejected,
        ] {
            let backend = FakeBackend {
                configure: Err(expected),
                requests: Mutex::new(VecDeque::from([Err(expected)])),
                channels: Mutex::new(Vec::new()),
            };
            assert_eq!(backend.configure(&0x4321), Err(expected));
            assert_eq!(backend.request(&0x4321), Err(expected));
        }
    }

    #[test]
    fn owned_request_accepts_only_the_exact_device_call_and_channel() {
        let backend = FakeBackend {
            configure: Ok(()),
            requests: Mutex::new(VecDeque::from([Ok(CallCompletionTicket { core_id: 81 })])),
            channels: Mutex::new(Vec::new()),
        };
        assert_eq!(
            request_owned_with(
                &backend,
                ownership("SEP001", 41, Some("SEP001"), Some(41)),
                Some(&0x7777),
            ),
            Ok(CallCompletionTicket { core_id: 81 })
        );
        assert_eq!(*backend.channels.lock().unwrap(), vec![0x7777]);

        for invalid in [
            ownership("SEP001", 41, Some("SEP002"), Some(41)),
            ownership("SEP001", 41, Some("SEP001"), Some(42)),
            ownership("SEP001", 41, None, None),
        ] {
            assert_eq!(
                request_owned_with(&backend, invalid, Some(&0x7777)),
                Err(CallCompletionError::Unavailable)
            );
        }
        assert_eq!(
            request_owned_with(
                &backend,
                ownership("SEP001", 41, Some("SEP001"), Some(41)),
                None,
            ),
            Err(CallCompletionError::Unavailable)
        );
        assert_eq!(*backend.channels.lock().unwrap(), vec![0x7777]);
    }

    #[test]
    fn failures_have_exact_handset_prompts_and_duplicate_is_not_success() {
        for (error, prompt) in [
            (
                CallCompletionError::NoOffer,
                "Callback is not available for this call",
            ),
            (
                CallCompletionError::LimitReached,
                "Too many callback requests",
            ),
            (
                CallCompletionError::Unavailable,
                "Callback service is unavailable",
            ),
            (CallCompletionError::Rejected, "Callback request failed"),
        ] {
            assert_eq!(error.handset_prompt(), prompt);
        }

        let backend = FakeBackend {
            configure: Ok(()),
            requests: Mutex::new(VecDeque::from([
                Ok(CallCompletionTicket { core_id: 82 }),
                Err(CallCompletionError::NoOffer),
            ])),
            channels: Mutex::new(Vec::new()),
        };
        let owned = ownership("SEP001", 41, Some("SEP001"), Some(41));
        assert!(request_owned_with(&backend, owned, Some(&0x9999)).is_ok());
        assert_eq!(
            request_owned_with(&backend, owned, Some(&0x9999)),
            Err(CallCompletionError::NoOffer)
        );
        assert_eq!(*backend.channels.lock().unwrap(), vec![0x9999, 0x9999]);
    }

    #[test]
    fn channel_tech_target_collapses_shared_appearances_and_rejects_ambiguity() {
        assert_eq!(
            canonical_callback_target(["1001".to_owned(), "1001".to_owned()]),
            Ok("SCCP/1001".to_owned())
        );
        assert_eq!(
            canonical_callback_target(["1001".to_owned(), "1002".to_owned()]),
            Err(CallCompletionTargetError::Ambiguous)
        );
        assert_eq!(
            canonical_callback_target(Vec::<String>::new()),
            Err(CallCompletionTargetError::Unavailable)
        );
    }
}
