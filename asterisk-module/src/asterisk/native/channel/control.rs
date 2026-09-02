//! Typed channel control and routing operations.

use std::ffi::{CStr, c_char, c_int};
use std::mem;
use std::ptr::{self, NonNull};
use std::time::Duration;

use crate::asterisk::raw::handles::{Ao2Object, BorrowedChannelLock as ChannelLock};
use crate::asterisk::sys;

use super::allocation::{channel_private, private_ownership};
use super::ownership::HangupOwnership;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelControl {
    Ringing,
    Answer,
    Hold,
    Unhold,
    VideoUpdate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelOperationError {
    Rejected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttendedTransferResult {
    Success,
    NotPermitted,
    Invalid,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TonePair {
    frequencies_hz: [u16; 2],
    duration: Duration,
    volume: u16,
}

impl TonePair {
    pub const fn new(
        first_hz: u16,
        second_hz: Option<u16>,
        duration: Duration,
        volume: u16,
    ) -> Self {
        let second_hz = match second_hz {
            Some(frequency) => frequency,
            None => 0,
        };
        Self {
            frequencies_hz: [first_hz, second_hz],
            duration,
            volume,
        }
    }
}

pub unsafe fn start_tone_pair(
    channel: NonNull<sys::ast_channel>,
    tone: TonePair,
) -> Result<(), ChannelOperationError> {
    let duration_ms =
        c_int::try_from(tone.duration.as_millis()).map_err(|_| ChannelOperationError::Rejected)?;
    (unsafe {
        sys::ast_tonepair_start(
            channel.as_ptr(),
            c_int::from(tone.frequencies_hz[0]),
            c_int::from(tone.frequencies_hz[1]),
            duration_ms,
            c_int::from(tone.volume),
        )
    } == 0)
        .then_some(())
        .ok_or(ChannelOperationError::Rejected)
}

pub unsafe fn stop_tone_pair(channel: NonNull<sys::ast_channel>) {
    unsafe { sys::ast_tonepair_stop(channel.as_ptr()) };
}

/// Start Asterisk-generated music on hold on this channel.
///
/// A missing or empty class lets Asterisk select the channel's configured
/// class and then the system default, matching its built-in channel drivers.
pub unsafe fn start_music_on_hold(
    channel: NonNull<sys::ast_channel>,
    class: Option<&CStr>,
) -> Result<(), ChannelOperationError> {
    let class = class
        .filter(|class| !class.to_bytes().is_empty())
        .map_or(ptr::null(), |class| class.as_ptr());
    (unsafe { sys::ast_moh_start(channel.as_ptr(), class, ptr::null()) } == 0)
        .then_some(())
        .ok_or(ChannelOperationError::Rejected)
}

/// Stop Asterisk-generated music on hold on this channel.
pub unsafe fn stop_music_on_hold(channel: NonNull<sys::ast_channel>) {
    unsafe { sys::ast_moh_stop(channel.as_ptr()) };
}

pub unsafe fn uniqueid_in_use(uniqueid: &CStr) -> bool {
    let channel = unsafe { sys::ast_channel_get_by_uniqueid(uniqueid.as_ptr()) };
    unsafe { Ao2Object::from_owned(channel) }.is_some()
}

pub unsafe fn queue_control(
    channel: NonNull<sys::ast_channel>,
    control: ChannelControl,
) -> Result<(), ChannelOperationError> {
    let control = match control {
        ChannelControl::Ringing => sys::AST_CONTROL_RINGING,
        ChannelControl::Answer => sys::AST_CONTROL_ANSWER,
        ChannelControl::Hold => sys::AST_CONTROL_HOLD,
        ChannelControl::Unhold => sys::AST_CONTROL_UNHOLD,
        ChannelControl::VideoUpdate => sys::AST_CONTROL_VIDUPDATE,
    };
    (unsafe { sys::ast_queue_control(channel.as_ptr(), control) } == 0)
        .then_some(())
        .ok_or(ChannelOperationError::Rejected)
}

/// Publish the native ringing state after an endpoint presentation request has
/// been accepted.
///
/// The technology callback owns the channel lock for this operation.
pub unsafe fn start_ringing(
    channel: NonNull<sys::ast_channel>,
) -> Result<(), ChannelOperationError> {
    unsafe { sys::ast_setstate(channel.as_ptr(), sys::AST_STATE_RINGING) };
    unsafe { queue_control(channel, ChannelControl::Ringing) }
}

pub unsafe fn hangup(
    channel: NonNull<sys::ast_channel>,
    cause: c_int,
) -> Result<(), ChannelOperationError> {
    let ownership = {
        let _lock = unsafe { ChannelLock::acquire(channel) }
            .map_err(|_| ChannelOperationError::Rejected)?;
        let private =
            unsafe { channel_private(channel.as_ptr()) }.ok_or(ChannelOperationError::Rejected)?;
        unsafe { sys::ast_channel_hangupcause_set(channel.as_ptr(), cause) };
        unsafe { private_ownership(&private) }.claim_hangup()
    };
    unsafe { sys::ast_set_hangupsource(channel.as_ptr(), c"SCCP".as_ptr(), 0) };
    match ownership {
        Ok(HangupOwnership::Hard) => {
            // `ast_hangup` consumes the module-owned allocation reference and
            // synchronously invokes the technology destructor. Do not touch
            // `private` or channel technology state after this call.
            unsafe { sys::ast_hangup(channel.as_ptr()) };
            Ok(())
        }
        Ok(HangupOwnership::Queued) => {
            (unsafe { sys::ast_queue_hangup_with_cause(channel.as_ptr(), cause) } == 0)
                .then_some(())
                .ok_or(ChannelOperationError::Rejected)
        }
        Ok(HangupOwnership::AlreadyStarted) => Ok(()),
        Err(_) => Err(ChannelOperationError::Rejected),
    }
}

pub unsafe fn queue_digit(
    channel: NonNull<sys::ast_channel>,
    digit: u8,
    duration_ms: u32,
) -> Result<(), ChannelOperationError> {
    let mut frame = unsafe { mem::zeroed::<sys::ast_frame>() };
    frame.frametype = sys::AST_FRAME_DTMF_END;
    frame.subclass.integer = c_char::from_ne_bytes([digit]) as c_int;
    frame.len = i64::from(duration_ms);
    (unsafe { sys::ast_queue_frame(channel.as_ptr(), &mut frame) } == 0)
        .then_some(())
        .ok_or(ChannelOperationError::Rejected)
}

pub unsafe fn start_dialplan(
    channel: NonNull<sys::ast_channel>,
    context: &CStr,
    extension: &CStr,
) -> Result<(), ChannelOperationError> {
    let lock =
        unsafe { ChannelLock::acquire(channel) }.map_err(|_| ChannelOperationError::Rejected)?;
    let private =
        unsafe { channel_private(channel.as_ptr()) }.ok_or(ChannelOperationError::Rejected)?;
    let ownership = unsafe { private_ownership(&private) }
        .begin_pbx_start()
        .map_err(|_| ChannelOperationError::Rejected)?;
    let previous_context =
        unsafe { CStr::from_ptr(sys::ast_channel_context(channel.as_ptr())) }.to_owned();
    let previous_extension =
        unsafe { CStr::from_ptr(sys::ast_channel_exten(channel.as_ptr())) }.to_owned();
    let previous_state = unsafe { sys::ast_channel_state(channel.as_ptr()) };
    unsafe {
        sys::ast_channel_context_set(channel.as_ptr(), context.as_ptr());
        sys::ast_channel_exten_set(channel.as_ptr(), extension.as_ptr());
        sys::ast_setstate(channel.as_ptr(), sys::AST_STATE_RING);
    }
    drop(lock);
    if unsafe { sys::ast_pbx_start(channel.as_ptr()) } == sys::AST_PBX_SUCCESS {
        return Ok(());
    }
    // No PBX worker was created, so the module must resume responsibility for
    // final destruction of a phone-originated collecting channel.
    {
        let _lock = unsafe { ChannelLock::acquire(channel) }
            .map_err(|_| ChannelOperationError::Rejected)?;
        let private =
            unsafe { channel_private(channel.as_ptr()) }.ok_or(ChannelOperationError::Rejected)?;
        unsafe { private_ownership(&private) }
            .rollback_pbx_start(ownership)
            .map_err(|_| ChannelOperationError::Rejected)?;
        unsafe {
            sys::ast_channel_context_set(channel.as_ptr(), previous_context.as_ptr());
            sys::ast_channel_exten_set(channel.as_ptr(), previous_extension.as_ptr());
            sys::ast_setstate(channel.as_ptr(), previous_state);
        }
    }
    Err(ChannelOperationError::Rejected)
}

pub unsafe fn attended_transfer(
    first: NonNull<sys::ast_channel>,
    second: NonNull<sys::ast_channel>,
) -> AttendedTransferResult {
    match unsafe { sys::ast_bridge_transfer_attended(first.as_ptr(), second.as_ptr()) } {
        value if value == sys::AST_BRIDGE_TRANSFER_SUCCESS => AttendedTransferResult::Success,
        value if value == sys::AST_BRIDGE_TRANSFER_NOT_PERMITTED => {
            AttendedTransferResult::NotPermitted
        }
        value if value == sys::AST_BRIDGE_TRANSFER_INVALID => AttendedTransferResult::Invalid,
        _ => AttendedTransferResult::Failed,
    }
}
