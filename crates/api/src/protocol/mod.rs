//! The wire protocol of the management API.
//!
//! The four pieces:
//!
//! * [`canonical`] — the byte-level string a request is signed over.
//! * [`signature`] — the Ed25519 signature and the embedded public key.
//! * [`nonce`] — the replay cache.
//! * [`auth`] — the four credentials and the checks they go through.
//! * [`error`] — the error envelope.

pub mod auth;
pub mod canonical;
pub mod error;
pub mod nonce;
pub mod signature;
