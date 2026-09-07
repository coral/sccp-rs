//! Backend-neutral controller state and effect execution ports.

pub mod backend;
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) mod call_queue;
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) mod callback_updates;
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) mod conference_announcement;
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) mod conference_tasks;
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) mod configuration_transaction;
pub mod controller;
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) mod mailbox;
#[cfg(any(feature = "asterisk-22", feature = "asterisk-latest", test))]
pub(crate) mod media_ownership;
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) mod owner;
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) mod parking_events;
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) mod publication;
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) mod resolver;
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) mod resource;
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) mod startup;
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) mod tls;

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) mod workers;
