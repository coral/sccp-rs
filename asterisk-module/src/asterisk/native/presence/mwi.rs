//! Asterisk MWI subscription and callback ownership.
//!
//! The boxed callback state remains at a stable address for Asterisk. Drop
//! unsubscribes and joins before releasing it, so no callback can observe
//! freed callback state. Callbacks emit owned updates without entering runtime state.

use std::ffi::{CString, c_void};
use std::ptr;
use std::sync::Arc;

use crate::asterisk::raw::registry::contain_callback_panic;
use crate::asterisk::sys;
use crate::presence::hints::HintError;

struct MwiState {
    subscriber: *mut sys::ast_mwi_subscriber,
    update: Arc<dyn Fn(bool) + Send + Sync>,
}

pub struct NativeMwiSubscription {
    state: Box<MwiState>,
}

// Asterisk owns callback execution and unsubscribe_and_join synchronizes it
// before the boxed callback state is released on the owning thread.
unsafe impl Send for NativeMwiSubscription {}

impl Drop for NativeMwiSubscription {
    fn drop(&mut self) {
        if !self.state.subscriber.is_null() {
            unsafe { sys::ast_mwi_unsubscribe_and_join(self.state.subscriber) };
            self.state.subscriber = ptr::null_mut();
        }
    }
}

unsafe extern "C" fn mwi_event(
    userdata: *mut c_void,
    _subscription: *mut sys::stasis_subscription,
    message: *mut sys::stasis_message,
) {
    if userdata.is_null() || message.is_null() {
        return;
    }
    contain_callback_panic((), || unsafe {
        if sys::ast_mwi_state_type() != sys::stasis_message_type(message) {
            return;
        }
        let state = sys::stasis_message_data(message).cast::<sys::ast_mwi_state>();
        if let Some(state) = state.as_ref() {
            let binding = &*userdata.cast::<MwiState>();
            (binding.update)(state.new_msgs > 0);
        }
    });
}

pub fn subscribe_mwi(
    mailbox: String,
    update: Arc<dyn Fn(bool) + Send + Sync>,
) -> Result<NativeMwiSubscription, HintError> {
    let mailbox = CString::new(mailbox).map_err(|_| HintError::InvalidText { field: "mailbox" })?;
    let mut state = Box::new(MwiState {
        subscriber: ptr::null_mut(),
        update,
    });
    state.subscriber = unsafe {
        sys::ast_mwi_subscribe_pool(
            mailbox.as_ptr(),
            Some(mwi_event),
            (&mut *state as *mut MwiState).cast(),
        )
    };
    if state.subscriber.is_null() {
        return Err(HintError::SubscribeFailed);
    }
    Ok(NativeMwiSubscription { state })
}
