//! Asterisk channel adapter for the Asterisk-independent `sccp-protocol` server.
//!
//! The crate intentionally has a development mode that does not require an
//! Asterisk checkout. Production modules are built with exactly one of the
//! `asterisk-22` or `asterisk-latest` source features and against a configured
//! Asterisk source/build tree.
//!
//! Public modules follow domain ownership: [`call`], [`config`], [`media`],
//! [`conference`], [`presence`], and [`state`] contain policy and state;
//! [`ami`], [`http`], and [`pbx`] contain bounded integration contracts; and
//! [`runtime`] contains the backend-neutral controller and effect ports. The
//! private `asterisk` module is the production composition root.
//!
//! # Build modes
//!
//! - The default `development` feature keeps native Asterisk headers out of
//!   ordinary tests and documentation builds.
//! - `asterisk-22` builds the release artifact against Asterisk 22 and accepts
//!   numeric Asterisk versions 22 or newer at startup.
//! - `asterisk-latest` follows upstream Asterisk master for compatibility
//!   testing and loads only into the exact master revision used to build it.
//! - The crate emits `libchan_sccp2.so`; installation renames that Cargo output
//!   to `chan_sccp2.so`. Bindgen's configured Asterisk ABI remains private to
//!   the Rust-native adapter.
//!
//! # Documentation map
//!
//! Configuration normalization and reload guarantees live in [`config`]. The
//! exact management, HTTP, and dialplan surfaces live in [`ami`], [`http`], and
//! [`pbx`]. Conference and media ownership rules live in [`conference`] and
//! [`media`]. This keeps operational contracts beside the code that enforces
//! their bounds, ordering, privacy, rollback, and lifetime rules.

pub mod ami;
pub mod call;
pub mod conference;
pub mod config;
pub mod http;
pub mod media;
pub mod pbx;
pub mod presence;
pub mod runtime;
pub mod state;

#[cfg(any(feature = "asterisk-22", feature = "asterisk-latest"))]
mod asterisk;

#[cfg(all(test, feature = "development", feature = "telemetry"))]
#[allow(dead_code)]
#[path = "asterisk/telemetry/capture.rs"]
mod telemetry_capture_tests;

#[cfg(all(feature = "asterisk-22", feature = "asterisk-latest"))]
compile_error!("select only one Asterisk ABI lane");

#[cfg(all(
    feature = "development",
    any(feature = "asterisk-22", feature = "asterisk-latest")
))]
compile_error!("disable the default development feature when building an Asterisk module");

#[cfg(all(
    feature = "live-asterisk-tests",
    not(any(feature = "asterisk-22", feature = "asterisk-latest"))
))]
compile_error!("live Asterisk tests require one Asterisk ABI lane");
