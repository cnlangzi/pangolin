//! The wire protocol of the dashboard channel.
//!
//! The four pieces:
//!
//! * [`canonical`] — the byte-level string a request is signed over.
//! * [`signature`] — the two signatures of the channel (the node's
//!   HMAC and the dashboard's Ed25519) and the dashboard key.
//! * [`nonce`] — the replay cache.
//! * [`error`] — the error envelope.
//!
//! The **authentication** of the channel lives in two directions and
//! nowhere else: the node signs its requests with the shared token
//! (HMAC-SHA256), the dashboard signs its responses with its Ed25519
//! key, and the node verifies that key against the one compiled into
//! the binary.

use std::time::{SystemTime, UNIX_EPOCH};

pub mod canonical;
pub mod error;
pub mod nonce;
pub mod signature;

/// The current Unix time, in seconds.
///
/// The timestamp of a request is part of its canonical string, so both
/// sides have to agree on what "now" means — down to the second.
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_is_a_unix_timestamp_in_seconds() {
        let now = now_secs();
        // Later than 2020 and earlier than 2100 — a broken clock would
        // put every request outside the timestamp window.
        assert!(now > 1_577_836_800, "{now}");
        assert!(now < 4_102_444_800, "{now}");
    }
}
