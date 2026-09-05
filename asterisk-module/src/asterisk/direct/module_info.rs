//! Rust-owned Asterisk module descriptor and ELF registration hooks.

use std::ffi::{CStr, c_int};
use std::mem;
use std::ptr;

use crate::asterisk::boundary::contain_panic as callback_guard;
use crate::asterisk::sys;

use super::channel_driver;
use crate::asterisk::StaticDescriptor;

const MODULE_NAME: &[u8] = b"chan_sccp2\0";
const DESCRIPTION: &[u8] = b"Rust SCCP Channel Driver\0";
const OPTIONAL_MODULES: &[u8] = b"res_sorcery_astdb,app_mixmonitor\0";
const GPL_KEY: &[u8] = b"This paragraph is copyright (c) 2006 by Digium, Inc. In order for your module to load, it must return this key via a function called \"key\".  Any code which includes this paragraph must be licensed under the GNU General Public License version 2 or later (at your option).  In addition to Digium's general reservations of rights, Digium expressly reserves the right to allow other parties to license this paragraph under different terms. Any use of Digium, Inc. trademarks or logos (including \"Asterisk\" or \"Digium\") without express written permission of Digium, Inc. is prohibited.\n\0";

static MODULE_INFO: StaticDescriptor<sys::ast_module_info> = StaticDescriptor::uninit();

unsafe extern "C" fn load() -> sys::ast_module_load_result {
    callback_guard(sys::AST_MODULE_LOAD_DECLINE, || {
        if !running_asterisk_matches_lane() {
            return sys::AST_MODULE_LOAD_DECLINE;
        }
        channel_driver::load()
            .map(|()| sys::AST_MODULE_LOAD_SUCCESS)
            .unwrap_or(sys::AST_MODULE_LOAD_DECLINE)
    })
}

unsafe extern "C" fn unload() -> c_int {
    callback_guard(-1, || {
        crate::asterisk::boundary::CallbackStatus::from_result(channel_driver::unload()).as_raw()
    })
}

unsafe extern "C" fn reload() -> c_int {
    callback_guard(-1, || {
        crate::asterisk::boundary::CallbackStatus::from_result(channel_driver::reload()).as_raw()
    })
}

fn numeric_major(version: &[u8]) -> Option<u32> {
    let major = version
        .split(|byte| *byte == b'.')
        .next()
        .filter(|major| !major.is_empty())?;
    std::str::from_utf8(major).ok()?.parse().ok()
}

fn version_matches_lane(version: &[u8]) -> bool {
    match env!("SCCP_ASTERISK_LANE") {
        "22" => numeric_major(version).is_some_and(|major| major >= 22),
        "latest" => version == env!("SCCP_ASTERISK_VERSION").as_bytes(),
        _ => false,
    }
}

fn running_asterisk_matches_lane() -> bool {
    // SAFETY: Asterisk owns this static NUL-terminated version string for the
    // lifetime of the process.
    let version = unsafe { sys::ast_get_version() };
    !version.is_null() && version_matches_lane(unsafe { CStr::from_ptr(version) }.to_bytes())
}

unsafe extern "C" fn register_module() {
    callback_guard((), || {
        // Zero initialization exactly mirrors the C designated initializer:
        // reserved pointers and optional dependency strings remain null.
        let mut info = unsafe { mem::zeroed::<sys::ast_module_info>() };
        info.load = Some(load);
        info.reload = Some(reload);
        info.unload = Some(unload);
        info.name = MODULE_NAME.as_ptr().cast();
        info.description = DESCRIPTION.as_ptr().cast();
        info.key = GPL_KEY.as_ptr().cast();
        info.flags = sys::AST_MODFLAG_LOAD_ORDER;
        // An empty sum is Asterisk's supported opt-out from its exact build
        // option checksum. Release modules intentionally span distribution
        // builds and patch releases within the explicitly checked ABI lane.
        info.buildopt_sum = [0; 33];
        info.load_pri = sys::AST_MODPRI_CHANNEL_DRIVER as u8;
        info.optional_modules = OPTIONAL_MODULES.as_ptr().cast();
        info.support_level = sys::AST_MODULE_SUPPORT_EXTENDED;

        // SAFETY: the ELF constructor is the unique initializer, the storage
        // address is stable for the life of the DSO, and Asterisk retains it
        // until the matching destructor unregisters it.
        let info = unsafe { MODULE_INFO.write(info) };
        unsafe { sys::ast_module_register(info) };
    });
}

unsafe extern "C" fn unregister_module() {
    callback_guard((), || {
        // SAFETY: If the platform unmaps the DSO, Asterisk runs this destructor
        // after the module lifecycle and registration initialized the descriptor
        // at this stable address. glibc may instead retain the image while Rust
        // TLS destructors remain pending; in that case the descriptor remains
        // registered as Not Running and a later load restarts this same image.
        // Replacing the binary therefore requires an Asterisk process restart.
        unsafe { sys::ast_module_unregister(MODULE_INFO.as_ptr()) };
    });
}

// These arrays are the Rust equivalent of Asterisk's constructor/destructor
// attributes used by AST_MODULE_INFO.
#[cfg_attr(target_os = "linux", unsafe(link_section = ".init_array"))]
#[used]
static REGISTER_MODULE: unsafe extern "C" fn() = register_module;

#[cfg_attr(target_os = "linux", unsafe(link_section = ".fini_array"))]
#[used]
static UNREGISTER_MODULE: unsafe extern "C" fn() = unregister_module;

/// Asterisk's macros use this symbol to obtain the loader-populated module
/// handle when registering owned resources.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __internal_chan_sccp2_self() -> *mut sys::ast_module {
    callback_guard(ptr::null_mut(), || {
        // SAFETY: the loader calls this only after registration initialized the
        // descriptor; reading the loader-owned `self` field is atomic within
        // the serialized module lifecycle.
        unsafe { (*MODULE_INFO.as_ptr()).self_ }
    })
}

pub unsafe fn module_self() -> *mut sys::ast_module {
    // SAFETY: callers use this only while the module is registered.
    unsafe { __internal_chan_sccp2_self() }
}

#[cfg(test)]
mod tests {
    use super::version_matches_lane;

    #[cfg(feature = "asterisk-22")]
    #[test]
    fn release_lane_accepts_its_baseline_and_newer_majors() {
        assert!(version_matches_lane(b"22.0.0"));
        assert!(version_matches_lane(b"22.99.1-rc1"));
        assert!(version_matches_lane(b"23.0.0"));
        assert!(!version_matches_lane(b"21.99.0"));
        for malformed in [b"".as_slice(), b"dev", b"22beta", b".22"] {
            assert!(!version_matches_lane(malformed));
        }
    }

    #[cfg(feature = "asterisk-latest")]
    #[test]
    fn latest_lane_accepts_only_the_compiled_master_identity() {
        let version = env!("SCCP_ASTERISK_VERSION");
        assert!(version.starts_with("GIT-master-"));
        assert!(version_matches_lane(version.as_bytes()));
        assert!(!version_matches_lane(b"GIT-master-different"));
        assert!(!version_matches_lane(b"24.0.0"));
    }
}
