//! Rust-owned Asterisk channel technology, RTP glue, and CLI descriptors.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::mem;
use std::ptr::{self, NonNull};
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::asterisk::boundary::{
    CallbackStatus, contain_panic as callback_guard, optional_c_text, read_c_int, required_c_text,
    write_c_int,
};
use crate::asterisk::native_channel::{
    audio_capability_mask, channel_private as private, destroy_channel_private,
    prepare_channel_private_teardown, private_owner, private_rtp, private_video_rtp,
    reassign_private_owner, retain_private_rtp, retain_private_video_rtp, start_music_on_hold,
    stop_music_on_hold, video_capability_mask,
};
use crate::asterisk::raw::system::device_state_raw;
use crate::asterisk::sys;
use crate::call::auto_answer::InboundDialRequest;
use crate::call::completion::canonical_callback_target;

use super::super::exports::{
    ChannelIndication, ChannelOperationError, ChannelRequest, ChannelRequestError, ChannelSecurity,
    DirectMediaPeer, MediaPeerUpdate, ModuleLifecycleError, RequestedChannel, answer_channel,
    channel_security, direct_media_allowed, fixup_channel, hangup_channel, has_active_channels,
    indicate_channel, line_device_state, place_call, reload_module, request_channel,
    resume_channel_operations, send_digit_begin_to_channel, send_digit_end_to_channel,
    send_text_to_channel, set_channel_audio_format, start_module, stop_module,
    suspend_channel_operations, update_rtp_peer,
};
use super::handles::{NativeChannelRegistration, TemporarilyUnlockedChannel};
use super::module_info::module_self;
use crate::asterisk::StaticDescriptor;

const SCCP_TYPE: &CStr = c"SCCP";
const SCCP_DESCRIPTION: &[u8] = b"Modern Cisco SCCP channel driver\0";
const SOURCE_FILE: &CStr = c"asterisk/direct/channel_driver.rs";
const SOURCE_FUNCTION: &CStr = c"sccp_channel_driver";
const CC_GENERIC_MONITOR: &CStr = c"generic";
const MAX_MUSIC_ON_HOLD_CLASS_BYTES: usize = 128;

static SCCP_TECH: StaticDescriptor<sys::ast_channel_tech> = StaticDescriptor::uninit();
static RTP_GLUE: StaticDescriptor<sys::ast_rtp_glue> = StaticDescriptor::uninit();
static NATIVE_REGISTRATION: Mutex<Option<NativeChannelRegistration>> = Mutex::new(None);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ChannelDriverLoadError;

fn native_registration() -> MutexGuard<'static, Option<NativeChannelRegistration>> {
    NATIVE_REGISTRATION
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Returns the stable channel-technology descriptor after module startup has
/// initialized it. Native channel allocation is only called after that point.
pub unsafe fn technology_ptr() -> *mut sys::ast_channel_tech {
    unsafe { SCCP_TECH.as_ptr() }
}

/// Returns the running module scheduler required by Asterisk RTP instances.
/// Channel allocation is unavailable until native registration has installed
/// this owner, and unload is rejected while any channel remains active.
pub fn rtp_scheduler() -> Option<NonNull<sys::ast_sched_context>> {
    native_registration()
        .as_ref()
        .map(NativeChannelRegistration::rtp_scheduler)
}

fn request_from_asterisk(
    capabilities: *mut sys::ast_format_cap,
    assigned_ids: *const sys::ast_assigned_ids,
    requestor: *const sys::ast_channel,
    address: &CStr,
) -> Result<RequestedChannel, ChannelRequestError> {
    unsafe {
        request_channel(ChannelRequest {
            capabilities,
            assigned_ids,
            requestor,
            address,
        })
    }
}

unsafe extern "C" fn requester_with_stream_topology(
    type_: *const c_char,
    topology: *mut sys::ast_stream_topology,
    assigned_ids: *const sys::ast_assigned_ids,
    requestor: *const sys::ast_channel,
    address: *const c_char,
    cause: *mut c_int,
) -> *mut sys::ast_channel {
    callback_guard(ptr::null_mut(), || unsafe {
        let _ = type_;
        let Ok(address) = required_c_text(address, 256) else {
            return ptr::null_mut();
        };
        let Ok(address) = CString::new(address) else {
            return ptr::null_mut();
        };
        if topology.is_null() {
            return ptr::null_mut();
        }
        let Some(capabilities) = NonNull::new(sys::ast_stream_topology_get_formats(topology))
        else {
            return ptr::null_mut();
        };
        let result =
            request_from_asterisk(capabilities.as_ptr(), assigned_ids, requestor, &address);
        crate::asterisk::native_channel::release_format_cap(capabilities);
        match result {
            Ok(requested) => {
                if let Some(value) = requested.cause
                    && !cause.is_null()
                {
                    *cause = value;
                }
                requested.channel.as_ptr().cast()
            }
            Err(error) => {
                if let Some(value) = error.cause
                    && !cause.is_null()
                {
                    *cause = value;
                }
                ptr::null_mut()
            }
        }
    })
}

unsafe extern "C" fn call(
    channel: *mut sys::ast_channel,
    address: *const c_char,
    timeout: c_int,
) -> c_int {
    callback_guard(-1, || unsafe {
        let _ = (address, timeout);
        CallbackStatus::from_result(
            NonNull::new(channel)
                .ok_or(())
                .and_then(|channel| place_call(channel).map_err(|_| ())),
        )
        .as_raw()
    })
}

unsafe extern "C" fn hangup(channel: *mut sys::ast_channel) -> c_int {
    callback_guard(-1, || unsafe {
        let Some(channel) = NonNull::new(channel) else {
            return -1;
        };
        let private = private(channel.as_ptr());
        let result = CallbackStatus::from_result(hangup_channel(channel).map_err(|_| ())).as_raw();
        if let Some(private) = private {
            prepare_channel_private_teardown(channel, private);
        }
        sys::ast_channel_tech_pvt_set(channel.as_ptr(), ptr::null_mut());
        if let Some(private) = private {
            destroy_channel_private(private);
        }
        result
    })
}

unsafe extern "C" fn answer(channel: *mut sys::ast_channel) -> c_int {
    callback_guard(-1, || unsafe {
        let result = NonNull::new(channel).ok_or(()).and_then(|channel| {
            let receipt = answer_channel(channel).map_err(|_| ())?;
            {
                let _unlocked = TemporarilyUnlockedChannel::new(channel);
                receipt.wait().map_err(|_| ())
            }
        });
        if result.is_ok() {
            sys::ast_setstate(channel, sys::AST_STATE_UP);
        }
        CallbackStatus::from_result(result).as_raw()
    })
}

unsafe extern "C" fn read(channel: *mut sys::ast_channel) -> *mut sys::ast_frame {
    callback_guard(ptr::null_mut(), || unsafe {
        let Some(private) = private(channel) else {
            return ptr::addr_of_mut!(sys::ast_null_frame);
        };
        let rtp = private_rtp(private);
        match sys::ast_channel_fdno(channel) {
            0 => sys::ast_rtp_instance_read(rtp.as_ptr(), 0),
            1 => sys::ast_rtp_instance_read(rtp.as_ptr(), 1),
            2 => private_video_rtp(private).map_or(ptr::addr_of_mut!(sys::ast_null_frame), |rtp| {
                sys::ast_rtp_instance_read(rtp.as_ptr(), 0)
            }),
            3 => private_video_rtp(private).map_or(ptr::addr_of_mut!(sys::ast_null_frame), |rtp| {
                sys::ast_rtp_instance_read(rtp.as_ptr(), 1)
            }),
            _ => ptr::addr_of_mut!(sys::ast_null_frame),
        }
    })
}

unsafe extern "C" fn write(channel: *mut sys::ast_channel, frame: *mut sys::ast_frame) -> c_int {
    callback_guard(-1, || unsafe {
        if frame.is_null() {
            return -1;
        }
        let Some(private) = private(channel) else {
            return -1;
        };
        let rtp = if (*frame).frametype == sys::AST_FRAME_VIDEO {
            let Some(video) = private_video_rtp(private) else {
                return 0;
            };
            video
        } else {
            private_rtp(private)
        };
        sys::ast_rtp_instance_write(rtp.as_ptr(), frame)
    })
}

unsafe extern "C" fn get_rtp_info(
    channel: *mut sys::ast_channel,
    instance: *mut *mut sys::ast_rtp_instance,
) -> sys::ast_rtp_glue_result {
    callback_guard(sys::AST_RTP_GLUE_RESULT_FORBID, || unsafe {
        let Some(private) = private(channel) else {
            return sys::AST_RTP_GLUE_RESULT_FORBID;
        };
        if instance.is_null() {
            return sys::AST_RTP_GLUE_RESULT_FORBID;
        }
        let rtp = retain_private_rtp(private);
        *instance = rtp;
        if direct_media_allowed(NonNull::new_unchecked(channel)) {
            sys::AST_RTP_GLUE_RESULT_REMOTE
        } else {
            sys::AST_RTP_GLUE_RESULT_LOCAL
        }
    })
}

unsafe extern "C" fn get_vrtp_info(
    channel: *mut sys::ast_channel,
    instance: *mut *mut sys::ast_rtp_instance,
) -> sys::ast_rtp_glue_result {
    callback_guard(sys::AST_RTP_GLUE_RESULT_FORBID, || unsafe {
        let Some(private) = private(channel) else {
            return sys::AST_RTP_GLUE_RESULT_FORBID;
        };
        if instance.is_null() {
            return sys::AST_RTP_GLUE_RESULT_FORBID;
        }
        let Some(rtp) = retain_private_video_rtp(private) else {
            return sys::AST_RTP_GLUE_RESULT_FORBID;
        };
        *instance = rtp;
        sys::AST_RTP_GLUE_RESULT_LOCAL
    })
}

unsafe fn update_peer_from_asterisk(
    channel: NonNull<sys::ast_channel>,
    instance: NonNull<sys::ast_rtp_instance>,
    capabilities: Option<NonNull<sys::ast_format_cap>>,
    nat_active: bool,
) -> Result<(), ChannelOperationError> {
    let mut remote = unsafe { mem::zeroed::<sys::ast_sockaddr>() };
    unsafe { sys::ast_rtp_instance_get_requested_target_address(instance.as_ptr(), &mut remote) };
    let port = if remote.len == 0 {
        0
    } else {
        unsafe {
            sys::_ast_sockaddr_port(
                &remote,
                SOURCE_FILE.as_ptr().cast(),
                line!() as c_int,
                SOURCE_FUNCTION.as_ptr().cast(),
            )
        }
    };
    let address = if port == 0 {
        None
    } else {
        let address = unsafe {
            sys::ast_sockaddr_stringify_fmt(&remote, sys::AST_SOCKADDR_STR_ADDR as c_int)
        };
        unsafe { optional_c_text(address, 64) }
            .ok()
            .flatten()
            .and_then(|address| address.parse().ok())
    };
    unsafe {
        update_rtp_peer(
            channel,
            MediaPeerUpdate::Direct(DirectMediaPeer {
                address,
                port,
                audio_capabilities: capabilities
                    .map(|capabilities| audio_capability_mask(Some(capabilities)).bits())
                    .unwrap_or(0),
                video_capabilities: capabilities
                    .map(|capabilities| video_capability_mask(Some(capabilities)).bits())
                    .unwrap_or(0),
                nat_active,
            }),
        )
    }
}

unsafe extern "C" fn update_peer(
    channel: *mut sys::ast_channel,
    instance: *mut sys::ast_rtp_instance,
    video: *mut sys::ast_rtp_instance,
    _text: *mut sys::ast_rtp_instance,
    capabilities: *const sys::ast_format_cap,
    nat_active: c_int,
) -> c_int {
    callback_guard(-1, || unsafe {
        let result = NonNull::new(channel).ok_or(()).and_then(|channel| {
            if let Some(instance) = NonNull::new(instance) {
                update_peer_from_asterisk(
                    channel,
                    instance,
                    NonNull::new(capabilities.cast_mut()),
                    nat_active != 0,
                )
                .map_err(|_| ())
            } else if !video.is_null() {
                // Video glue is always local, so a video-only peer refresh
                // leaves the independently routed audio stream unchanged.
                Ok(())
            } else {
                update_rtp_peer(channel, MediaPeerUpdate::Anchor).map_err(|_| ())
            }
        });
        CallbackStatus::from_result(result).as_raw()
    })
}

unsafe extern "C" fn get_codec(channel: *mut sys::ast_channel, result: *mut sys::ast_format_cap) {
    callback_guard((), || unsafe {
        if !channel.is_null() && !result.is_null() {
            sys::ast_format_cap_append_from_cap(
                result,
                sys::ast_channel_nativeformats(channel),
                sys::AST_MEDIA_TYPE_UNKNOWN,
            );
        }
    });
}

unsafe extern "C" fn indicate(
    channel: *mut sys::ast_channel,
    condition: c_int,
    data: *const c_void,
    data_length: usize,
) -> c_int {
    callback_guard(-1, || unsafe {
        let Some(channel) = NonNull::new(channel) else {
            return -1;
        };
        if condition as u32 == sys::AST_CONTROL_MASQUERADE_NOTIFY {
            let Ok(beginning) = read_c_int(data, data_length) else {
                return -1;
            };
            let beginning = beginning != 0;
            let result = if beginning {
                let _unlocked = TemporarilyUnlockedChannel::new(channel);
                suspend_channel_operations(channel)
            } else {
                resume_channel_operations(channel)
            };
            return CallbackStatus::from_result(result.map_err(|_| ())).as_raw();
        }
        if condition as u32 == sys::AST_CONTROL_HOLD {
            let music_class = match music_on_hold_class(data, data_length) {
                Ok(music_class) => music_class,
                Err(()) => return -1,
            };
            return CallbackStatus::from_result(
                start_music_on_hold(channel, music_class.as_deref()).map_err(|_| ()),
            )
            .as_raw();
        }
        if condition as u32 == sys::AST_CONTROL_UNHOLD {
            stop_music_on_hold(channel);
            return 0;
        }
        let indication = match condition {
            -1 => ChannelIndication::StopTone,
            value if value as u32 == sys::AST_CONTROL_INCOMPLETE => ChannelIndication::Incomplete,
            value if value as u32 == sys::AST_CONTROL_SRCUPDATE => ChannelIndication::SourceUpdate,
            value if value as u32 == sys::AST_CONTROL_SRCCHANGE => ChannelIndication::SourceChange,
            value if value as u32 == sys::AST_CONTROL_UPDATE_RTP_PEER => {
                ChannelIndication::UpdateRtpPeer
            }
            value if value as u32 == sys::AST_CONTROL_VIDUPDATE => ChannelIndication::VideoUpdate,
            value if value as u32 == sys::AST_CONTROL_RINGING => ChannelIndication::Ringing,
            value if value as u32 == sys::AST_CONTROL_ANSWER => ChannelIndication::Answer,
            value if value as u32 == sys::AST_CONTROL_BUSY => ChannelIndication::Busy,
            value if value as u32 == sys::AST_CONTROL_CONGESTION => ChannelIndication::Congestion,
            value if value as u32 == sys::AST_CONTROL_PROGRESS => ChannelIndication::Progress,
            value if value as u32 == sys::AST_CONTROL_PROCEEDING => ChannelIndication::Proceeding,
            value if value as u32 == sys::AST_CONTROL_CONNECTED_LINE => {
                ChannelIndication::ConnectedLine
            }
            value if value as u32 == sys::AST_CONTROL_REDIRECTING => ChannelIndication::Redirecting,
            _ => return -1,
        };
        CallbackStatus::from_result(indicate_channel(channel, indication).map_err(|_| ())).as_raw()
    })
}

fn music_on_hold_class(data: *const c_void, data_length: usize) -> Result<Option<CString>, ()> {
    if data_length == 0 {
        return Ok(None);
    }
    if data.is_null() || data_length > MAX_MUSIC_ON_HOLD_CLASS_BYTES + 1 {
        return Err(());
    }
    let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), data_length) };
    CStr::from_bytes_with_nul(bytes)
        .map(|class| (!class.to_bytes().is_empty()).then(|| class.to_owned()))
        .map_err(|_| ())
}

unsafe extern "C" fn send_digit_begin(channel: *mut sys::ast_channel, digit: c_char) -> c_int {
    callback_guard(-1, || unsafe {
        let Some(digit) = dtmf_digit(digit) else {
            return -1;
        };
        CallbackStatus::from_result(
            NonNull::new(channel)
                .ok_or(())
                .and_then(|channel| send_digit_begin_to_channel(channel, digit).map_err(|_| ())),
        )
        .as_raw()
    })
}

fn dtmf_digit(digit: c_char) -> Option<u8> {
    let digit = digit as u8;
    matches!(digit, b'0'..=b'9' | b'*' | b'#' | b'A'..=b'D').then_some(digit)
}

unsafe extern "C" fn send_text(channel: *mut sys::ast_channel, text: *const c_char) -> c_int {
    callback_guard(-1, || unsafe {
        let result = NonNull::new(channel).ok_or(()).and_then(|channel| {
            required_c_text(text, 1_024)
                .map_err(|_| ())
                .and_then(|text| send_text_to_channel(channel, text).map_err(|_| ()))
        });
        CallbackStatus::from_result(result).as_raw()
    })
}

unsafe extern "C" fn send_digit_end(
    channel: *mut sys::ast_channel,
    digit: c_char,
    duration: u32,
) -> c_int {
    callback_guard(-1, || unsafe {
        let Some(digit) = dtmf_digit(digit) else {
            return -1;
        };
        let result = NonNull::new(channel).ok_or(()).and_then(|channel| {
            send_digit_end_to_channel(
                channel,
                digit,
                std::time::Duration::from_millis(duration.into()),
            )
            .map_err(|_| ())
        });
        CallbackStatus::from_result(result).as_raw()
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SecurityOption {
    Signaling,
    Media,
}

impl TryFrom<c_int> for SecurityOption {
    type Error = ();

    fn try_from(value: c_int) -> Result<Self, Self::Error> {
        match value as u32 {
            sys::AST_OPTION_SECURE_SIGNALING => Ok(Self::Signaling),
            sys::AST_OPTION_SECURE_MEDIA => Ok(Self::Media),
            _ => Err(()),
        }
    }
}

impl SecurityOption {
    const fn enabled(self, security: ChannelSecurity) -> bool {
        match self {
            Self::Signaling => security.signaling,
            Self::Media => security.media,
        }
    }
}

unsafe extern "C" fn set_option(
    channel: *mut sys::ast_channel,
    option: c_int,
    data: *mut c_void,
    data_length: c_int,
) -> c_int {
    callback_guard(-1, || unsafe {
        let Some(channel) = NonNull::new(channel) else {
            return -1;
        };
        if matches!(
            option as u32,
            sys::AST_OPTION_FORMAT_READ | sys::AST_OPTION_FORMAT_WRITE
        ) {
            if usize::try_from(data_length).ok() != Some(mem::size_of::<*mut sys::ast_format>())
                || data.is_null()
            {
                return -1;
            }
            let requested = data.cast::<*mut sys::ast_format>().read_unaligned();
            let result = NonNull::new(requested)
                .ok_or(())
                .and_then(|requested| set_channel_audio_format(channel, requested).map_err(|_| ()));
            return CallbackStatus::from_result(result).as_raw();
        }
        let Ok(option) = SecurityOption::try_from(option) else {
            return -1;
        };
        let result = usize::try_from(data_length)
            .ok()
            .and_then(|length| read_c_int(data, length).ok())
            .ok_or(())
            .and_then(|requested| {
                let security = channel_security(channel).map_err(|_| ())?;
                (option.enabled(security) == (requested != 0))
                    .then_some(())
                    .ok_or(())
            });
        CallbackStatus::from_result(result).as_raw()
    })
}

unsafe extern "C" fn query_option(
    channel: *mut sys::ast_channel,
    option: c_int,
    data: *mut c_void,
    data_length: *mut c_int,
) -> c_int {
    callback_guard(-1, || unsafe {
        let (Some(channel), Ok(option)) = (NonNull::new(channel), SecurityOption::try_from(option))
        else {
            return -1;
        };
        let result = channel_security(channel)
            .map_err(|_| ())
            .and_then(|security| {
                write_c_int(data, data_length, c_int::from(option.enabled(security)))
                    .map_err(|_| ())
            });
        CallbackStatus::from_result(result).as_raw()
    })
}

unsafe extern "C" fn fixup(
    old_channel: *mut sys::ast_channel,
    new_channel: *mut sys::ast_channel,
) -> c_int {
    callback_guard(-1, || unsafe {
        let (Some(old_channel), Some(new_channel)) =
            (NonNull::new(old_channel), NonNull::new(new_channel))
        else {
            return -1;
        };
        let Some(private) = private(new_channel.as_ptr()) else {
            return -1;
        };
        if private_owner(private) != Some(old_channel) {
            return -1;
        }
        if fixup_channel(old_channel, new_channel).is_err() {
            return -1;
        }
        reassign_private_owner(private, new_channel);
        let uniqueid = sys::ast_channel_uniqueid(new_channel.as_ptr());
        sys::ast_rtp_instance_set_channel_id(private_rtp(private).as_ptr(), uniqueid);
        if let Some(video) = private_video_rtp(private) {
            sys::ast_rtp_instance_set_channel_id(video.as_ptr(), uniqueid);
        }
        0
    })
}

unsafe extern "C" fn device_state(line: *const c_char) -> c_int {
    callback_guard(sys::AST_DEVICE_UNKNOWN as c_int, || unsafe {
        let Ok(line) = required_c_text(line, 256) else {
            return sys::AST_DEVICE_UNKNOWN as c_int;
        };
        device_state_raw(line_device_state(&line)) as c_int
    })
}

struct CallCompletionParameters(NonNull<sys::ast_cc_config_params>);

impl CallCompletionParameters {
    fn new() -> Option<Self> {
        NonNull::new(unsafe {
            sys::__ast_cc_config_params_init(
                SOURCE_FILE.as_ptr().cast(),
                line!() as c_int,
                SOURCE_FUNCTION.as_ptr().cast(),
            )
        })
        .map(Self)
    }

    fn as_ptr(&self) -> *mut sys::ast_cc_config_params {
        self.0.as_ptr()
    }
}

impl Drop for CallCompletionParameters {
    fn drop(&mut self) {
        unsafe { sys::ast_cc_config_params_destroy(self.0.as_ptr()) };
    }
}

fn callback_target(destination: &CStr) -> Result<CString, ()> {
    let destination = destination.to_str().map_err(|_| ())?;
    let access = crate::asterisk::runtime::module_access().ok_or(())?;
    let request = InboundDialRequest::parse(destination).map_err(|_| ())?;
    let target = canonical_callback_target(
        access
            .inbound_line_bindings(request.target())
            .into_iter()
            .map(|binding| binding.line.number),
    )
    .map_err(|_| ())?;
    let target = CString::new(target).map_err(|_| ())?;
    if target.as_bytes_with_nul().len() > sys::AST_CHANNEL_NAME as usize {
        return Err(());
    }
    Ok(target)
}

unsafe fn register_completion_monitor(
    inbound: NonNull<sys::ast_channel>,
    destination: &CStr,
    callback: sys::ast_cc_callback_fn,
) -> Result<(), ()> {
    let Some(callback) = callback else {
        return Err(());
    };
    let target = callback_target(destination)?;
    let parameters = CallCompletionParameters::new().ok_or(())?;
    if unsafe { sys::ast_set_cc_monitor_policy(parameters.as_ptr(), sys::AST_CC_MONITOR_GENERIC) }
        != 0
    {
        return Err(());
    }
    unsafe {
        callback(
            inbound.as_ptr(),
            parameters.as_ptr(),
            CC_GENERIC_MONITOR.as_ptr().cast(),
            target.as_ptr(),
            target.as_ptr(),
            ptr::null_mut(),
        );
    }
    Ok(())
}

unsafe extern "C" fn call_completion(
    inbound: *mut sys::ast_channel,
    destination: *const c_char,
    callback: sys::ast_cc_callback_fn,
) -> c_int {
    callback_guard(-1, || unsafe {
        if callback.is_none() {
            return -1;
        }
        let Some(inbound) = NonNull::new(inbound) else {
            return -1;
        };
        let Ok(destination) = required_c_text(destination, 256) else {
            return -1;
        };
        let Ok(destination) = CString::new(destination) else {
            return -1;
        };
        CallbackStatus::from_result(register_completion_monitor(inbound, &destination, callback))
            .as_raw()
    })
}

fn channel_technology() -> sys::ast_channel_tech {
    let mut technology = unsafe { mem::zeroed::<sys::ast_channel_tech>() };
    technology.type_ = SCCP_TYPE.as_ptr().cast();
    technology.description = SCCP_DESCRIPTION.as_ptr().cast();
    technology.properties =
        (sys::AST_CHAN_TP_WANTSJITTER | sys::AST_CHAN_TP_CREATESJITTER) as c_int;
    technology.requester_with_stream_topology = Some(requester_with_stream_topology);
    technology.devicestate = Some(device_state);
    technology.send_digit_begin = Some(send_digit_begin);
    technology.send_digit_end = Some(send_digit_end);
    technology.send_text = Some(send_text);
    technology.setoption = Some(set_option);
    technology.queryoption = Some(query_option);
    technology.call = Some(call);
    technology.hangup = Some(hangup);
    technology.answer = Some(answer);
    technology.read = Some(read);
    technology.write = Some(write);
    technology.write_video = Some(write);
    technology.exception = Some(read);
    technology.indicate = Some(indicate);
    technology.fixup = Some(fixup);
    technology.cc_callback = Some(call_completion);
    technology
}

fn rtp_glue() -> sys::ast_rtp_glue {
    let mut glue = unsafe { mem::zeroed::<sys::ast_rtp_glue>() };
    glue.type_ = SCCP_TYPE.as_ptr().cast();
    glue.get_rtp_info = Some(get_rtp_info);
    glue.get_vrtp_info = Some(get_vrtp_info);
    glue.update_peer = Some(update_peer);
    glue.get_codec = Some(get_codec);
    glue
}

unsafe fn technology_formats() -> impl Iterator<Item = *mut sys::ast_format> {
    unsafe {
        [
            sys::ast_format_ulaw,
            sys::ast_format_alaw,
            sys::ast_format_g722,
            sys::ast_format_g723,
            sys::ast_format_g729,
            sys::ast_format_g726_aal2,
            sys::ast_format_gsm,
            sys::ast_format_slin16,
            sys::ast_format_ilbc,
            sys::ast_format_siren7,
            sys::ast_format_opus,
            sys::ast_format_h261,
            sys::ast_format_h263,
            sys::ast_format_h263p,
            sys::ast_format_h264,
        ]
    }
    .into_iter()
}

pub(super) fn load() -> Result<(), ChannelDriverLoadError> {
    let mut native_registration = native_registration();
    if native_registration.is_some() {
        return Ok(());
    }
    let (technology, glue, cli) = unsafe {
        let technology = NonNull::new_unchecked(SCCP_TECH.write(channel_technology()));
        let glue = NonNull::new_unchecked(RTP_GLUE.write(rtp_glue()));
        let cli = super::cli::entries();
        (technology, glue, cli)
    };

    if start_module().is_err() {
        crate::asterisk::raw::dialplan::cleanup();
        return Err(ChannelDriverLoadError);
    }
    let registration = unsafe {
        NativeChannelRegistration::register(
            technology,
            glue,
            cli,
            module_self(),
            technology_formats(),
        )
    };
    let Some(registration) = registration else {
        let _ = stop_module();
        crate::asterisk::raw::dialplan::cleanup();
        return Err(ChannelDriverLoadError);
    };
    *native_registration = Some(registration);
    Ok(())
}

pub(super) fn unload() -> Result<(), ModuleLifecycleError> {
    if has_active_channels() {
        return Err(ModuleLifecycleError);
    }
    let registration = native_registration().take();
    drop(registration);
    let result = stop_module();
    crate::asterisk::raw::dialplan::cleanup();
    result
}

pub(super) fn reload() -> Result<(), ModuleLifecycleError> {
    reload_module()
}
