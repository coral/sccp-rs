//! Backend-neutral effects produced by the controller and their execution boundary.
//!
//! Effects are ordered transactions, not an eventually consistent command
//! list. In particular, ordinary [`PbxEffect::ConfigureMedia`] updates the PBX
//! endpoint and returns the handset transmit request that must complete before
//! the next effect. [`PbxEffect::ConfigureMediaOnly`] is reserved for an
//! already-coupled early-media transaction and deliberately emits no duplicate
//! transmit request. Cleanup callers attempt every terminal effect even after
//! an individual backend or handset failure.

use std::fmt;
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
use std::ops::BitOrAssign;

use sccp_protocol::{
    CallId, CallInfo, CallState as HandsetCallState, Codec, ConferenceId, ConferenceListEntry,
    DeviceId, MediaEndpoint, ParticipantId, PassthroughPartyId, SessionGeneration, Tone,
};

use super::controller::ConferenceMutationToken;
use crate::call::forwarding::ForwardingOperation;
use crate::call::transfer::TransferCompletion;
use crate::call::voicemail::VoicemailOperation;
use crate::config::LineBinding;
use crate::media::encryption::LocalEncryptionCapabilities;

mod contracts;
mod execution;
pub use contracts::{PbxBackendError, PbxServiceCapabilities};
pub use execution::EffectExecutionError;

mod effects;
mod traits;
pub use effects::*;
pub use traits::*;

/// Execute effects sequentially and stop at the first failed backend or
/// handset operation. A media backend result is delivered to the handset
/// immediately before the next queued effect.
pub async fn execute_effects<Backend, SendHandset, SendFuture, HandsetError>(
    backend: &Backend,
    effects: Vec<DriverEffect>,
    send_handset: SendHandset,
) -> Result<(), EffectExecutionError<Backend::Error, HandsetError>>
where
    Backend: PbxBackend,
    SendHandset: FnMut(HandsetEffect) -> SendFuture,
    SendFuture: std::future::Future<Output = Result<(), HandsetError>>,
{
    execution::execute_effects(backend, effects, send_handset).await
}

/// Execute terminal cleanup effects in order while attempting every queued
/// operation. Once the controller has committed terminal state, later cleanup
/// must not be skipped because an earlier native or handset target vanished.
pub async fn execute_cleanup_effects<Backend, SendHandset, SendFuture, HandsetError>(
    backend: &Backend,
    effects: Vec<DriverEffect>,
    send_handset: SendHandset,
) -> Vec<EffectExecutionError<Backend::Error, HandsetError>>
where
    Backend: PbxBackend,
    SendHandset: FnMut(HandsetEffect) -> SendFuture,
    SendFuture: std::future::Future<Output = Result<(), HandsetError>>,
{
    execution::execute_cleanup_effects(backend, effects, send_handset).await
}

#[cfg(test)]
#[path = "backend/tests/mod.rs"]
mod tests;
