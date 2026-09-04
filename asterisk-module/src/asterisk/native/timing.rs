//! Owned wrappers for Asterisk calendar timing expressions.

use std::ffi::CString;
use std::mem::MaybeUninit;

use thiserror::Error;

use crate::asterisk::sys;

/// A parsed Asterisk timing expression.
///
/// The expression is compiled once and then only read by
/// [`sys::ast_check_timing`]. Asterisk owns the optional timezone allocation
/// inside the record, so destruction must always go through
/// [`sys::ast_destroy_timing`].
pub struct AsteriskTiming {
    inner: sys::ast_timing,
}

impl AsteriskTiming {
    pub fn parse(expression: &str) -> Result<Self, AsteriskTimingError> {
        let expression = CString::new(expression).map_err(AsteriskTimingError::InvalidText)?;
        let mut inner = MaybeUninit::<sys::ast_timing>::zeroed();
        let result = unsafe { sys::ast_build_timing(inner.as_mut_ptr(), expression.as_ptr()) };
        if result == 0 {
            return Err(AsteriskTimingError::Rejected);
        }
        Ok(Self {
            inner: unsafe { inner.assume_init() },
        })
    }

    pub fn matches_now(&self) -> bool {
        unsafe { sys::ast_check_timing(&self.inner) != 0 }
    }
}

#[derive(Debug, Error)]
pub enum AsteriskTimingError {
    #[error("timing expression contains a NUL byte")]
    InvalidText(#[source] std::ffi::NulError),
    #[error("Asterisk rejected the timing expression")]
    Rejected,
}

// ast_check_timing reads only the masks and immutable timezone string created
// by ast_build_timing. Arc ownership delays ast_destroy_timing until no caller
// can still be evaluating the record.
unsafe impl Send for AsteriskTiming {}
unsafe impl Sync for AsteriskTiming {}

impl Drop for AsteriskTiming {
    fn drop(&mut self) {
        unsafe {
            sys::ast_destroy_timing(&mut self.inner);
        }
    }
}
