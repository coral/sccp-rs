//! Native Asterisk CLI descriptors, parsing, completion, and handlers.

use std::ffi::{CStr, CString, c_char, c_int};
use std::mem;
use std::ptr::{self, NonNull};

use crate::ami::cli::{CliInventoryCommand, MAX_CLI_ARGUMENT_BYTES, MAX_CLI_ARGUMENTS};
use crate::ami::controls::{MAX_DEVICE_SELECTOR_BYTES, ResetMode};
use crate::ami::diagnostics::{
    CliDiagnosticCommand, MAX_CLI_DIAGNOSTIC_ARGUMENT_BYTES, MAX_CLI_DIAGNOSTIC_ARGUMENTS,
};
use crate::asterisk::StaticDescriptor;
use crate::asterisk::boundary::{
    contain_panic as callback_guard, optional_c_text, required_c_text,
};
use crate::asterisk::sys;
use crate::config::reload::{MAX_RELOAD_ARGUMENT_BYTES, MAX_RELOAD_ARGUMENTS};

use super::super::exports::{
    ControlCliCommand, complete_control_cli, complete_device_control_cli, complete_diagnostic_cli,
    complete_dnd_schedule_cli, complete_inventory_cli, complete_reload_cli, execute_control_cli,
    execute_device_control_cli, execute_diagnostic_cli, execute_dnd_schedule_cli,
    execute_forwarding_cli, execute_inventory_cli, execute_reload_cli, execute_version_cli,
};

#[cfg(not(feature = "live-asterisk-tests"))]
const CLI_ENTRY_COUNT: usize = 17;
#[cfg(feature = "live-asterisk-tests")]
const CLI_ENTRY_COUNT: usize = 18;

static CLI_ENTRIES: StaticDescriptor<[sys::ast_cli_entry; CLI_ENTRY_COUNT]> =
    StaticDescriptor::uninit();

enum CliPhase {
    Initialize,
    Generate,
    Execute,
}

impl CliPhase {
    const fn from_raw(command: c_int) -> Self {
        match command {
            -2 => Self::Initialize,
            -3 => Self::Generate,
            _ => Self::Execute,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CliDisposition {
    Complete,
    ShowUsage,
}

fn cli_disposition_pointer(disposition: CliDisposition) -> *mut c_char {
    match disposition {
        CliDisposition::Complete => ptr::null_mut(),
        CliDisposition::ShowUsage => ptr::dangling_mut::<c_char>(),
    }
}

#[derive(Clone, Copy)]
struct CliArgs<'a> {
    raw: &'a sys::ast_cli_args,
}

#[derive(Debug, Eq, PartialEq)]
struct CliInvocation {
    fd: c_int,
    arguments: Vec<String>,
}

#[derive(Debug, Eq, PartialEq)]
struct CliCompletion {
    position: usize,
    ordinal: usize,
    prefix: String,
    arguments: Vec<String>,
}

impl<'a> CliArgs<'a> {
    unsafe fn from_raw(arguments: *mut sys::ast_cli_args) -> Option<Self> {
        NonNull::new(arguments).map(|arguments| Self {
            raw: unsafe { arguments.as_ref() },
        })
    }

    fn argument_pointer(self, index: usize) -> Result<*const c_char, ()> {
        let count = usize::try_from(self.raw.argc).map_err(|_| ())?;
        if index >= count || self.raw.argv.is_null() {
            return Err(());
        }
        Ok(unsafe { *self.raw.argv.add(index) })
    }

    fn required_argument(self, index: usize, maximum_bytes: usize) -> Result<String, ()> {
        let argument = self.argument_pointer(index)?;
        unsafe { required_c_text(argument, maximum_bytes) }.map_err(|_| ())
    }

    fn optional_argument(self, index: usize, maximum_bytes: usize) -> Result<Option<String>, ()> {
        let argument = self.argument_pointer(index)?;
        unsafe { optional_c_text(argument, maximum_bytes) }.map_err(|_| ())
    }

    fn prefix(self, maximum_bytes: usize) -> Result<String, ()> {
        unsafe { optional_c_text(self.raw.word, maximum_bytes) }
            .map(Option::unwrap_or_default)
            .map_err(|_| ())
    }

    fn invocation(
        self,
        command_words: usize,
        accepts_count: impl FnOnce(usize) -> bool,
        argument_bound: impl Fn(usize) -> Option<usize>,
    ) -> Result<CliInvocation, ()> {
        let argument_count = usize::try_from(self.raw.argc)
            .ok()
            .and_then(|count| count.checked_sub(command_words))
            .ok_or(())?;
        if !accepts_count(argument_count) || (argument_count != 0 && self.raw.argv.is_null()) {
            return Err(());
        }
        let arguments = (0..argument_count)
            .map(|index| {
                self.required_argument(index + command_words, argument_bound(index).ok_or(())?)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(CliInvocation {
            fd: self.raw.fd,
            arguments,
        })
    }

    fn completion_cursor(
        self,
        command_words: usize,
        prefix_bound: impl FnOnce(usize) -> Option<usize>,
    ) -> Result<CliCompletion, ()> {
        let position = usize::try_from(self.raw.pos).map_err(|_| ())?;
        let ordinal = usize::try_from(self.raw.n).map_err(|_| ())?;
        let argument_count = position.checked_sub(command_words).ok_or(())?;
        let prefix = self.prefix(prefix_bound(argument_count).ok_or(())?)?;
        Ok(CliCompletion {
            position,
            ordinal,
            prefix,
            arguments: Vec::new(),
        })
    }

    fn completion(
        self,
        command_words: usize,
        accepts_previous_count: impl FnOnce(usize) -> bool,
        argument_bound: impl Fn(usize) -> Option<usize>,
    ) -> Result<CliCompletion, ()> {
        let mut completion = self.completion_cursor(command_words, &argument_bound)?;
        let argument_count = completion.position - command_words;
        if !accepts_previous_count(argument_count)
            || (argument_count != 0 && self.raw.argv.is_null())
        {
            return Err(());
        }
        completion.arguments = (0..argument_count)
            .map(|index| {
                self.required_argument(index + command_words, argument_bound(index).ok_or(())?)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(completion)
    }
}

fn cli_completion(candidate: Option<String>) -> *mut c_char {
    candidate
        .and_then(|candidate| CString::new(candidate).ok())
        .map_or(ptr::null_mut(), |candidate| {
            crate::asterisk::raw::system::cli_completion(&candidate)
        })
}

unsafe fn run_version_cli(
    entry: Option<NonNull<sys::ast_cli_entry>>,
    phase: CliPhase,
    arguments: Option<CliArgs<'_>>,
) -> CliDisposition {
    match phase {
        CliPhase::Initialize => {
            if let Some(mut entry) = entry {
                unsafe {
                    entry.as_mut().command = c"sccp version".as_ptr().cast_mut();
                    entry.as_mut().usage = c"Usage: sccp version\n".as_ptr();
                }
            }
            CliDisposition::Complete
        }
        CliPhase::Generate => CliDisposition::Complete,
        CliPhase::Execute => {
            let Some(arguments) = arguments else {
                return CliDisposition::ShowUsage;
            };
            let Ok(invocation) = arguments.invocation(2, |count| count == 0, |_| None) else {
                return CliDisposition::ShowUsage;
            };
            execute_version_cli(invocation.fd);
            CliDisposition::Complete
        }
    }
}

unsafe extern "C" fn cli_version(
    entry: *mut sys::ast_cli_entry,
    command: c_int,
    arguments: *mut sys::ast_cli_args,
) -> *mut c_char {
    callback_guard(ptr::null_mut(), || unsafe {
        cli_disposition_pointer(run_version_cli(
            NonNull::new(entry),
            CliPhase::from_raw(command),
            CliArgs::from_raw(arguments),
        ))
    })
}

unsafe fn run_reload_cli(
    entry: Option<NonNull<sys::ast_cli_entry>>,
    phase: CliPhase,
    arguments: Option<CliArgs<'_>>,
) -> *mut c_char {
    match phase {
        CliPhase::Initialize => {
            if let Some(mut entry) = entry {
                unsafe {
                    entry.as_mut().command = c"sccp reload".as_ptr().cast_mut();
                    entry.as_mut().usage =
                        c"Usage: sccp reload [device <id>|line <number>|profile <name>]\n".as_ptr();
                }
            }
            ptr::null_mut()
        }
        CliPhase::Generate => {
            let Some(arguments) = arguments else {
                return ptr::null_mut();
            };
            let Ok(completion) = arguments.completion(
                2,
                |count| count < MAX_RELOAD_ARGUMENTS,
                |_| Some(MAX_RELOAD_ARGUMENT_BYTES),
            ) else {
                return ptr::null_mut();
            };
            cli_completion(complete_reload_cli(
                &completion.arguments,
                &completion.prefix,
                completion.ordinal,
            ))
        }
        CliPhase::Execute => {
            let Some(arguments) = arguments else {
                return cli_disposition_pointer(CliDisposition::ShowUsage);
            };
            let Ok(invocation) = arguments.invocation(
                2,
                |count| count <= MAX_RELOAD_ARGUMENTS,
                |_| Some(MAX_RELOAD_ARGUMENT_BYTES),
            ) else {
                return cli_disposition_pointer(CliDisposition::ShowUsage);
            };
            execute_reload_cli(invocation.fd, &invocation.arguments);
            ptr::null_mut()
        }
    }
}

unsafe extern "C" fn cli_reload(
    entry: *mut sys::ast_cli_entry,
    command: c_int,
    arguments: *mut sys::ast_cli_args,
) -> *mut c_char {
    callback_guard(ptr::null_mut(), || unsafe {
        run_reload_cli(
            NonNull::new(entry),
            CliPhase::from_raw(command),
            CliArgs::from_raw(arguments),
        )
    })
}

unsafe fn run_inventory_cli(
    entry: Option<NonNull<sys::ast_cli_entry>>,
    phase: CliPhase,
    arguments: Option<CliArgs<'_>>,
    operation: CliInventoryCommand,
    command_text: &'static CStr,
    usage: &'static CStr,
) -> *mut c_char {
    match phase {
        CliPhase::Initialize => {
            if let Some(mut entry) = entry {
                unsafe {
                    entry.as_mut().command = command_text.as_ptr().cast_mut();
                    entry.as_mut().usage = usage.as_ptr();
                }
            }
            ptr::null_mut()
        }
        CliPhase::Generate => {
            let Some(arguments) = arguments else {
                return ptr::null_mut();
            };
            let Ok(completion) = arguments.completion(
                3,
                |count| count <= MAX_CLI_ARGUMENTS,
                |_| Some(MAX_CLI_ARGUMENT_BYTES),
            ) else {
                return ptr::null_mut();
            };
            cli_completion(complete_inventory_cli(
                operation,
                &completion.arguments,
                &completion.prefix,
                completion.ordinal,
            ))
        }
        CliPhase::Execute => {
            let Some(arguments) = arguments else {
                return cli_disposition_pointer(CliDisposition::ShowUsage);
            };
            let Ok(invocation) = arguments.invocation(
                3,
                |count| count <= MAX_CLI_ARGUMENTS,
                |_| Some(MAX_CLI_ARGUMENT_BYTES),
            ) else {
                return cli_disposition_pointer(CliDisposition::ShowUsage);
            };
            execute_inventory_cli(invocation.fd, operation, &invocation.arguments);
            ptr::null_mut()
        }
    }
}

macro_rules! inventory_cli_handler {
    ($name:ident, $operation:expr, $command:expr, $usage:expr) => {
        unsafe extern "C" fn $name(
            entry: *mut sys::ast_cli_entry,
            command: c_int,
            arguments: *mut sys::ast_cli_args,
        ) -> *mut c_char {
            callback_guard(ptr::null_mut(), || unsafe {
                run_inventory_cli(
                    NonNull::new(entry),
                    CliPhase::from_raw(command),
                    CliArgs::from_raw(arguments),
                    $operation,
                    $command,
                    $usage,
                )
            })
        }
    };
}

inventory_cli_handler!(
    cli_devices,
    CliInventoryCommand::Devices,
    c"sccp show devices",
    c"Usage: sccp show devices [device [appearances [device:instance]|buttons [position]|capabilities [position]|features [name]]]\n"
);
inventory_cli_handler!(
    cli_lines,
    CliInventoryCommand::Lines,
    c"sccp show lines",
    c"Usage: sccp show lines [line [appearances [device:instance]]]\n"
);
inventory_cli_handler!(
    cli_channels,
    CliInventoryCommand::Channels,
    c"sccp show channels",
    c"Usage: sccp show channels [pbx-call-id]\n"
);

unsafe fn run_diagnostic_cli(
    entry: Option<NonNull<sys::ast_cli_entry>>,
    phase: CliPhase,
    arguments: Option<CliArgs<'_>>,
    operation: CliDiagnosticCommand,
    command_words: usize,
    command_text: &'static CStr,
    usage: &'static CStr,
) -> *mut c_char {
    match phase {
        CliPhase::Initialize => {
            if let Some(mut entry) = entry {
                unsafe {
                    entry.as_mut().command = command_text.as_ptr().cast_mut();
                    entry.as_mut().usage = usage.as_ptr();
                }
            }
            ptr::null_mut()
        }
        CliPhase::Generate => {
            let Some(arguments) = arguments else {
                return ptr::null_mut();
            };
            let Ok(completion) = arguments.completion(
                command_words,
                |count| count <= MAX_CLI_DIAGNOSTIC_ARGUMENTS,
                |_| Some(MAX_CLI_DIAGNOSTIC_ARGUMENT_BYTES),
            ) else {
                return ptr::null_mut();
            };
            cli_completion(complete_diagnostic_cli(
                operation,
                &completion.arguments,
                &completion.prefix,
                completion.ordinal,
            ))
        }
        CliPhase::Execute => {
            let Some(arguments) = arguments else {
                return cli_disposition_pointer(CliDisposition::ShowUsage);
            };
            let Ok(invocation) = arguments.invocation(
                command_words,
                |count| count <= MAX_CLI_DIAGNOSTIC_ARGUMENTS,
                |_| Some(MAX_CLI_DIAGNOSTIC_ARGUMENT_BYTES),
            ) else {
                return cli_disposition_pointer(CliDisposition::ShowUsage);
            };
            execute_diagnostic_cli(invocation.fd, operation, &invocation.arguments);
            ptr::null_mut()
        }
    }
}

macro_rules! diagnostic_cli_handler {
    ($name:ident, $operation:expr, $words:expr, $command:expr, $usage:expr) => {
        unsafe extern "C" fn $name(
            entry: *mut sys::ast_cli_entry,
            command: c_int,
            arguments: *mut sys::ast_cli_args,
        ) -> *mut c_char {
            callback_guard(ptr::null_mut(), || unsafe {
                run_diagnostic_cli(
                    NonNull::new(entry),
                    CliPhase::from_raw(command),
                    CliArgs::from_raw(arguments),
                    $operation,
                    $words,
                    $command,
                    $usage,
                )
            })
        }
    };
}

diagnostic_cli_handler!(
    cli_media,
    CliDiagnosticCommand::Media,
    3,
    c"sccp show media",
    c"Usage: sccp show media [pbx-call-id [call-id [audio|video [receive|transmit]]]]\n"
);
diagnostic_cli_handler!(
    cli_media_statistics,
    CliDiagnosticCommand::MediaStatistics,
    4,
    c"sccp show media statistics",
    c"Usage: sccp show media statistics [device [call-id]]\n"
);
diagnostic_cli_handler!(
    cli_sessions,
    CliDiagnosticCommand::Sessions,
    3,
    c"sccp show sessions",
    c"Usage: sccp show sessions [device]\n"
);

unsafe fn run_device_control_cli(
    entry: Option<NonNull<sys::ast_cli_entry>>,
    phase: CliPhase,
    arguments: Option<CliArgs<'_>>,
    mode: ResetMode,
    command_text: &'static CStr,
    usage: &'static CStr,
) -> *mut c_char {
    match phase {
        CliPhase::Initialize => {
            if let Some(mut entry) = entry {
                unsafe {
                    entry.as_mut().command = command_text.as_ptr().cast_mut();
                    entry.as_mut().usage = usage.as_ptr();
                }
            }
            ptr::null_mut()
        }
        CliPhase::Generate => {
            let Some(arguments) = arguments else {
                return ptr::null_mut();
            };
            let Ok(completion) = arguments
                .completion_cursor(2, |index| (index == 0).then_some(MAX_DEVICE_SELECTOR_BYTES))
            else {
                return ptr::null_mut();
            };
            cli_completion(complete_device_control_cli(
                &completion.prefix,
                completion.ordinal,
            ))
        }
        CliPhase::Execute => {
            let Some(arguments) = arguments else {
                return cli_disposition_pointer(CliDisposition::ShowUsage);
            };
            let Ok(invocation) =
                arguments.invocation(2, |count| count == 1, |_| Some(MAX_DEVICE_SELECTOR_BYTES))
            else {
                return cli_disposition_pointer(CliDisposition::ShowUsage);
            };
            let [device] = invocation.arguments.as_slice() else {
                return cli_disposition_pointer(CliDisposition::ShowUsage);
            };
            execute_device_control_cli(invocation.fd, device, mode);
            ptr::null_mut()
        }
    }
}

macro_rules! device_control_cli_handler {
    ($name:ident, $mode:expr, $command:expr, $usage:expr) => {
        unsafe extern "C" fn $name(
            entry: *mut sys::ast_cli_entry,
            command: c_int,
            arguments: *mut sys::ast_cli_args,
        ) -> *mut c_char {
            callback_guard(ptr::null_mut(), || unsafe {
                run_device_control_cli(
                    NonNull::new(entry),
                    CliPhase::from_raw(command),
                    CliArgs::from_raw(arguments),
                    $mode,
                    $command,
                    $usage,
                )
            })
        }
    };
}

device_control_cli_handler!(
    cli_reset,
    ResetMode::Reset,
    c"sccp reset",
    c"Usage: sccp reset <device|all>\n"
);
device_control_cli_handler!(
    cli_restart,
    ResetMode::Restart,
    c"sccp restart",
    c"Usage: sccp restart <device|all>\n"
);

unsafe fn run_control_cli(
    entry: Option<NonNull<sys::ast_cli_entry>>,
    phase: CliPhase,
    arguments: Option<CliArgs<'_>>,
    operation: ControlCliCommand,
    command_text: &'static CStr,
    usage: &'static CStr,
) -> *mut c_char {
    match phase {
        CliPhase::Initialize => {
            if let Some(mut entry) = entry {
                unsafe {
                    entry.as_mut().command = command_text.as_ptr().cast_mut();
                    entry.as_mut().usage = usage.as_ptr();
                }
            }
            ptr::null_mut()
        }
        CliPhase::Generate => {
            let Some(arguments) = arguments else {
                return ptr::null_mut();
            };
            let Ok(completion) =
                arguments.completion_cursor(2, |index| operation.argument_bound(index))
            else {
                return ptr::null_mut();
            };
            let context = if operation == ControlCliCommand::Originate && completion.position == 4 {
                arguments
                    .optional_argument(2, MAX_DEVICE_SELECTOR_BYTES)
                    .ok()
                    .flatten()
            } else {
                None
            };
            cli_completion(complete_control_cli(
                operation,
                completion.position,
                &completion.prefix,
                completion.ordinal,
                context.as_deref(),
            ))
        }
        CliPhase::Execute => {
            let Some(arguments) = arguments else {
                return cli_disposition_pointer(CliDisposition::ShowUsage);
            };
            let Ok(invocation) = arguments.invocation(
                2,
                |count| operation.accepts_argument_count(count),
                |index| operation.argument_bound(index),
            ) else {
                return cli_disposition_pointer(CliDisposition::ShowUsage);
            };
            execute_control_cli(invocation.fd, operation, &invocation.arguments);
            ptr::null_mut()
        }
    }
}

macro_rules! control_cli_handler {
    ($name:ident, $operation:expr, $command:expr, $usage:expr) => {
        unsafe extern "C" fn $name(
            entry: *mut sys::ast_cli_entry,
            command: c_int,
            arguments: *mut sys::ast_cli_args,
        ) -> *mut c_char {
            callback_guard(ptr::null_mut(), || unsafe {
                run_control_cli(
                    NonNull::new(entry),
                    CliPhase::from_raw(command),
                    CliArgs::from_raw(arguments),
                    $operation,
                    $command,
                    $usage,
                )
            })
        }
    };
}

control_cli_handler!(
    cli_dnd,
    ControlCliCommand::Dnd,
    c"sccp dnd",
    c"Usage: sccp dnd <device> <off|silent|reject>\n"
);

/// # Safety
///
/// Any supplied entry and arguments must remain valid Asterisk CLI callback
/// records for the duration of the call.
unsafe fn run_dnd_schedule_cli(
    entry: Option<NonNull<sys::ast_cli_entry>>,
    phase: CliPhase,
    arguments: Option<CliArgs<'_>>,
) -> *mut c_char {
    match phase {
        CliPhase::Initialize => {
            if let Some(mut entry) = entry {
                unsafe {
                    entry.as_mut().command = c"sccp dnd schedule".as_ptr().cast_mut();
                    entry.as_mut().usage = c"Usage:\n  sccp dnd schedule <device> show\n  sccp dnd schedule <device> add <HH:MM-HH:MM> <days> <silent|reject>\n  sccp dnd schedule <device> remove <index>\n  sccp dnd schedule <device> clear\n  sccp dnd schedule <device> reset\n".as_ptr();
                }
            }
            ptr::null_mut()
        }
        CliPhase::Generate => {
            let Some(arguments) = arguments else {
                return ptr::null_mut();
            };
            let Ok(completion) = arguments.completion_cursor(3, |_| Some(128)) else {
                return ptr::null_mut();
            };
            cli_completion(complete_dnd_schedule_cli(
                completion.position,
                &completion.prefix,
                completion.ordinal,
            ))
        }
        CliPhase::Execute => {
            let Some(arguments) = arguments else {
                return cli_disposition_pointer(CliDisposition::ShowUsage);
            };
            let Ok(invocation) =
                arguments.invocation(3, |count| (2..=5).contains(&count), |_| Some(128))
            else {
                return cli_disposition_pointer(CliDisposition::ShowUsage);
            };
            execute_dnd_schedule_cli(invocation.fd, &invocation.arguments);
            ptr::null_mut()
        }
    }
}

/// # Safety
///
/// Asterisk must supply live callback pointers matching its CLI ABI.
unsafe extern "C" fn cli_dnd_schedule(
    entry: *mut sys::ast_cli_entry,
    command: c_int,
    arguments: *mut sys::ast_cli_args,
) -> *mut c_char {
    callback_guard(ptr::null_mut(), || unsafe {
        run_dnd_schedule_cli(
            NonNull::new(entry),
            CliPhase::from_raw(command),
            CliArgs::from_raw(arguments),
        )
    })
}
control_cli_handler!(
    cli_message,
    ControlCliCommand::Message,
    c"sccp message",
    c"Usage: sccp message <device|all|system> <text> [yes|no] [timeout]\n"
);
control_cli_handler!(
    cli_answer,
    ControlCliCommand::Answer,
    c"sccp answer",
    c"Usage: sccp answer <call-id> [device]\n"
);
control_cli_handler!(
    cli_end,
    ControlCliCommand::End,
    c"sccp end",
    c"Usage: sccp end <call-id>\n"
);
control_cli_handler!(
    cli_originate,
    ControlCliCommand::Originate,
    c"sccp originate",
    c"Usage: sccp originate <device> <number> [line] [assigned-channel-id]\n"
);

unsafe fn run_forwarding_cli(
    entry: Option<NonNull<sys::ast_cli_entry>>,
    phase: CliPhase,
    arguments: Option<CliArgs<'_>>,
) -> CliDisposition {
    match phase {
        CliPhase::Initialize => {
            let Some(mut entry) = entry else {
                return CliDisposition::Complete;
            };
            unsafe {
                entry.as_mut().command = c"sccp set forwarding".as_ptr().cast_mut();
                entry.as_mut().usage = c"Usage: sccp set forwarding <device> <line> <all|busy|noanswer> <destination|off>\n".as_ptr();
            }
            CliDisposition::Complete
        }
        CliPhase::Generate => CliDisposition::Complete,
        CliPhase::Execute => {
            let Some(arguments) = arguments else {
                return CliDisposition::ShowUsage;
            };
            let Ok(invocation) = arguments.invocation(3, |count| count == 4, |_| Some(256)) else {
                return CliDisposition::ShowUsage;
            };
            let [device, line, kind, destination] = invocation.arguments.as_slice() else {
                return CliDisposition::ShowUsage;
            };
            execute_forwarding_cli(invocation.fd, device, line, kind, destination);
            CliDisposition::Complete
        }
    }
}

unsafe extern "C" fn cli_forwarding(
    entry: *mut sys::ast_cli_entry,
    command: c_int,
    arguments: *mut sys::ast_cli_args,
) -> *mut c_char {
    callback_guard(ptr::null_mut(), || unsafe {
        cli_disposition_pointer(run_forwarding_cli(
            NonNull::new(entry),
            CliPhase::from_raw(command),
            CliArgs::from_raw(arguments),
        ))
    })
}

fn cli_entry(
    summary: &'static [u8],
    handler: unsafe extern "C" fn(
        *mut sys::ast_cli_entry,
        c_int,
        *mut sys::ast_cli_args,
    ) -> *mut c_char,
) -> sys::ast_cli_entry {
    let mut entry = unsafe { mem::zeroed::<sys::ast_cli_entry>() };
    entry.summary = summary.as_ptr().cast();
    entry.handler = Some(handler);
    entry
}

pub(super) unsafe fn entries() -> NonNull<[sys::ast_cli_entry; CLI_ENTRY_COUNT]> {
    unsafe {
        NonNull::new_unchecked(CLI_ENTRIES.write([
            cli_entry(b"Show the SCCP module version\0", cli_version),
            cli_entry(b"Show registered SCCP devices\0", cli_devices),
            cli_entry(b"Show configured SCCP lines\0", cli_lines),
            cli_entry(b"Show active SCCP channels\0", cli_channels),
            cli_entry(b"Show correlated SCCP media\0", cli_media),
            cli_entry(
                b"Show correlated SCCP media statistics\0",
                cli_media_statistics,
            ),
            cli_entry(b"Show active SCCP sessions\0", cli_sessions),
            cli_entry(b"Reload SCCP configuration\0", cli_reload),
            cli_entry(b"Reset a registered SCCP device\0", cli_reset),
            cli_entry(b"Restart a registered SCCP device\0", cli_restart),
            cli_entry(b"Set DND on a registered SCCP device\0", cli_dnd),
            cli_entry(b"Manage recurring SCCP DND schedules\0", cli_dnd_schedule),
            cli_entry(b"Display a message on SCCP devices\0", cli_message),
            cli_entry(b"Answer a ringing SCCP call\0", cli_answer),
            cli_entry(b"End an SCCP call\0", cli_end),
            cli_entry(b"Originate an SCCP call\0", cli_originate),
            cli_entry(b"Set SCCP line forwarding\0", cli_forwarding),
            #[cfg(feature = "live-asterisk-tests")]
            crate::asterisk::raw::live_bridge_cli_entry(),
        ]))
    }
}
