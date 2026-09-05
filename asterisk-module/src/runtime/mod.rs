//! Backend-neutral controller state and effect execution ports.

pub mod backend;
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) mod conference_announcement;
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) mod conference_tasks;
pub mod controller;
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) mod resource;
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) mod tls;
