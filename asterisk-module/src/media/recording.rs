//! Owned call-recording values and backend-neutral recording ports.
//!
//! Runtime policy lives above this boundary: an administrative recording first
//! acquires a recording media-anchor lease and retargets any live direct path to
//! local RTP. Only then may MixMonitor start. A failed start releases the lease;
//! successful stop or unload closes the owned native session before releasing
//! it. This module contains no Asterisk ABI records or callback trampolines.

use std::fmt;
use std::sync::Arc;

use sccp_protocol::DeviceId;
use thiserror::Error;

use crate::runtime::backend::PbxCallId;

/// The current state of an owned recording session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingState {
    Active,
    Muted,
    Stopped,
}

/// The audio direction affected by a recording mute operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingDirection {
    Read,
    Write,
    Both,
}

/// A transition emitted by an owned recording session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingEvent {
    Started,
    Stopped,
    MuteChanged {
        direction: RecordingDirection,
        muted: bool,
        /// Number of MixMonitor audio hooks affected on the channel.
        affected: usize,
    },
}

/// State transition selected for an authorized recording toggle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingTogglePlan {
    Start,
    Stop,
}

/// Reason a device-scoped recording toggle cannot proceed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingToggleRejection {
    Ownership,
    CallState,
}

/// Selection of the file target for a new recording session.
///
/// Automatic targets are resolved by the native adapter from PBX-owned channel
/// identity. Explicit names come from administrative interfaces and remain
/// subject to their existing ingress validation before reaching this port.
#[derive(Clone, Eq, PartialEq)]
pub enum RecordingTarget {
    Automatic,
    ExplicitlyNamed(String),
}

impl fmt::Debug for RecordingTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Automatic => formatter.write_str("Automatic"),
            Self::ExplicitlyNamed(_) => formatter.write_str("ExplicitlyNamed(<redacted>)"),
        }
    }
}

/// Authorize a recording toggle without coupling the policy to a native session.
pub fn plan_recording_toggle(
    requested_device: &DeviceId,
    owner_device: &DeviceId,
    controllable: bool,
    active: bool,
) -> Result<RecordingTogglePlan, RecordingToggleRejection> {
    if requested_device != owner_device {
        return Err(RecordingToggleRejection::Ownership);
    }
    if active {
        return Ok(RecordingTogglePlan::Stop);
    }
    controllable
        .then_some(RecordingTogglePlan::Start)
        .ok_or(RecordingToggleRejection::CallState)
}

pub type RecordingCallback = Arc<dyn Fn(RecordingEvent) + Send + Sync + 'static>;

/// Backend-neutral control surface for one owned recording session.
pub trait RecordingSessionControl: Send {
    type Error;

    fn id(&self) -> Result<String, Self::Error>;
    fn state(&self) -> Result<RecordingState, Self::Error>;
    fn stop(&mut self) -> Result<(), Self::Error>;
    fn set_muted(
        &mut self,
        direction: RecordingDirection,
        muted: bool,
    ) -> Result<usize, Self::Error>;
}

/// Starts recording sessions for backend-neutral PBX call identities.
pub trait RecordingProvider {
    type Session: RecordingSessionControl;
    type StartError;

    fn start_recording(
        &self,
        call_id: PbxCallId,
        target: RecordingTarget,
        options: &str,
        callback: RecordingCallback,
    ) -> Result<Self::Session, Self::StartError>;
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) async fn ordered_recording_start<
    A,
    S,
    E,
    Confirmation,
    Start,
    Compensation,
    CompensationFuture,
>(
    confirmation: Confirmation,
    start: Start,
    compensate: Compensation,
) -> Result<(S, A), E>
where
    Confirmation: std::future::Future<Output = Result<A, E>>,
    Start: FnOnce() -> Result<S, E>,
    Compensation: FnOnce(A) -> CompensationFuture,
    CompensationFuture: std::future::Future<Output = ()>,
{
    let anchor = confirmation.await?;
    match start() {
        Ok(session) => Ok((session, anchor)),
        Err(error) => {
            compensate(anchor).await;
            Err(error)
        }
    }
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) async fn ordered_recording_stop<S, E, Stop, Restore, RestoreFuture>(
    mut session: S,
    stop: Stop,
    restore: Restore,
) -> Result<(), (E, S)>
where
    Stop: FnOnce(&mut S) -> Result<(), E>,
    Restore: FnOnce(S) -> RestoreFuture,
    RestoreFuture: std::future::Future<Output = Result<(), (E, S)>>,
{
    if let Err(error) = stop(&mut session) {
        return Err((error, session));
    }
    restore(session).await
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum RecordingError {
    #[error("{field} contains a NUL byte")]
    InvalidText { field: &'static str },

    #[error("recording filename must not be empty")]
    EmptyFilename,

    #[error("unable to start recording")]
    StartFailed,

    #[error("unable to stop recording")]
    StopFailed,

    #[error("unable to change recording mute state")]
    MuteFailed,

    #[error("Asterisk recording support is unavailable in development builds")]
    Unavailable,
}

pub fn validate_filename(value: &str) -> Result<(), RecordingError> {
    if value.is_empty() {
        return Err(RecordingError::EmptyFilename);
    }
    validate_text("filename", value)
}

pub fn validate_text(field: &'static str, value: &str) -> Result<(), RecordingError> {
    if value.contains('\0') {
        Err(RecordingError::InvalidText { field })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn validates_filename_and_options_before_native_dispatch() {
        assert_eq!(validate_filename(""), Err(RecordingError::EmptyFilename));
        assert_eq!(
            validate_filename("bad\0name"),
            Err(RecordingError::InvalidText { field: "filename" })
        );
        assert_eq!(
            validate_text("options", "b\0W"),
            Err(RecordingError::InvalidText { field: "options" })
        );
    }

    #[test]
    fn explicitly_named_recording_targets_redact_filenames_from_debug() {
        let target = RecordingTarget::ExplicitlyNamed("private-call.wav".into());

        let debug = format!("{target:?}");
        assert_eq!(debug, "ExplicitlyNamed(<redacted>)");
        assert!(!debug.contains("private-call.wav"));
    }

    #[test]
    fn recording_toggle_requires_exact_ownership_and_a_live_start_state() {
        let owner = DeviceId::new("SEP001122334455").unwrap();
        let peer = DeviceId::new("SEP001122334466").unwrap();

        assert_eq!(
            plan_recording_toggle(&peer, &owner, true, false),
            Err(RecordingToggleRejection::Ownership)
        );
        assert_eq!(
            plan_recording_toggle(&owner, &owner, false, false),
            Err(RecordingToggleRejection::CallState)
        );
        assert_eq!(
            plan_recording_toggle(&owner, &owner, true, false),
            Ok(RecordingTogglePlan::Start)
        );
        assert_eq!(
            plan_recording_toggle(&owner, &owner, false, true),
            Ok(RecordingTogglePlan::Stop)
        );
    }

    #[test]
    fn owned_events_preserve_direction_and_affected_count() {
        assert_eq!(
            RecordingEvent::MuteChanged {
                direction: RecordingDirection::Both,
                muted: true,
                affected: 2,
            },
            RecordingEvent::MuteChanged {
                direction: RecordingDirection::Both,
                muted: true,
                affected: 2,
            }
        );
    }

    #[tokio::test]
    async fn start_waits_for_delivery_and_compensates_a_native_failure() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let confirmation_events = Arc::clone(&events);
        let start_events = Arc::clone(&events);
        let compensation_events = Arc::clone(&events);

        let result = ordered_recording_start(
            async move {
                confirmation_events.lock().unwrap().push("confirmed");
                Ok::<_, &'static str>("anchor")
            },
            move || {
                start_events.lock().unwrap().push("start");
                Err::<(), _>("start")
            },
            move |anchor| async move {
                assert_eq!(anchor, "anchor");
                compensation_events.lock().unwrap().push("restore");
            },
        )
        .await;

        assert_eq!(result, Err("start"));
        assert_eq!(*events.lock().unwrap(), ["confirmed", "start", "restore"]);
    }

    #[tokio::test]
    async fn start_does_not_touch_the_native_session_before_delivery() {
        let started = Arc::new(Mutex::new(false));
        let confirmation_started = Arc::clone(&started);
        let native_started = Arc::clone(&started);
        let result = ordered_recording_start(
            async move {
                *confirmation_started.lock().unwrap() = true;
                Ok::<_, &'static str>("anchor")
            },
            move || {
                assert!(*native_started.lock().unwrap());
                Ok::<_, &'static str>("session")
            },
            |_| async {},
        )
        .await;

        assert_eq!(result, Ok(("session", "anchor")));
    }

    #[tokio::test]
    async fn stop_restores_only_after_the_native_session_stops() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let stop_events = Arc::clone(&events);
        let restore_events = Arc::clone(&events);
        let result = ordered_recording_stop(
            "session",
            move |_| {
                stop_events.lock().unwrap().push("stop");
                Ok::<_, &'static str>(())
            },
            move |session| async move {
                assert_eq!(session, "session");
                restore_events.lock().unwrap().push("restore");
                Ok(())
            },
        )
        .await;

        assert_eq!(result, Ok(()));
        assert_eq!(*events.lock().unwrap(), ["stop", "restore"]);
    }

    #[tokio::test]
    async fn stop_failure_keeps_ownership_and_skips_restore() {
        let restored = Arc::new(Mutex::new(false));
        let restore_attempted = Arc::clone(&restored);
        let result = ordered_recording_stop(
            "session",
            |_| Err("stop"),
            move |session| async move {
                *restore_attempted.lock().unwrap() = true;
                Err(("restore", session))
            },
        )
        .await;

        assert_eq!(result, Err(("stop", "session")));
        assert!(!*restored.lock().unwrap());
    }
}
