//! PBX-neutral dialplan contracts and narrow native Asterisk operations.
//!
//! [`query`] owns the allowlisted `SCCPDevice`, `SCCPLine`, and `SCCPChannel`
//! functions. Applications include codec preference replacement, called-party
//! override, and [`handset_message`] (`SCCPSetMessage`). They operate only on
//! exact module-owned channels and return typed errors for missing, malformed,
//! ambiguous, private, or unsupported data.
//!
//! [`dialplan`] owns the bounded native registration boundary. [`registration`]
//! owns registration-context extensions with reference-counted shared-line
//! targets and rollback-safe add/remove/reload behavior. [`operations`],
//! [`party`], and [`channel_metadata`] contain narrow owned Asterisk channel
//! operations and privacy-preserving metadata propagation.

pub mod call_indication;
pub mod channel_metadata;
pub mod dialplan;
pub mod handset_message;
pub mod operations;
pub mod party;
pub mod query;
pub mod registration;
