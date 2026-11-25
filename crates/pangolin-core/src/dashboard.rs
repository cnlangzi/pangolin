//! The state of the outbound dashboard channel (issue #262).
//!
//! The node dials **out** to the dashboard, so its connection is
//! something the operator can no longer read off the socket table —
//! the node has to publish it. This is the small, in-memory record the
//! outbound client writes and the settings page reads.
//!
//! The state is deliberately **not** in the database: it describes a
//! moment ("is the subscription up right now?"), not a fact about the
//! node's configuration. A restart empties it, and the channel
//! re-establishes itself within a second.

/// What the node's connection to the dashboard looks like.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DashboardStatus {
    /// Whether the node currently holds a **verified** subscription —
    /// one whose handshake passed the signature, the nonce and the
    /// timestamp checks.
    pub online: bool,
    /// When the last successful handshake happened, RFC-3339 UTC.
    pub last_handshake_at: Option<String>,
    /// The last error the channel ran into, if any.
    ///
    /// The message never carries the token: a credential must not
    /// reach the log.
    pub last_error: Option<String>,
    /// The fingerprint of the dashboard key this build trusts — the
    /// SHA-256 hex digest of the raw public key.
    ///
    /// The key itself lives in the API crate (the admin crate cannot
    /// depend on it — the dependency runs the other way round), so the
    /// node publishes the fingerprint and the settings page shows it:
    /// an operator has to be able to see **which** key the node
    /// trusts.
    pub key_fingerprint: String,
}

impl DashboardStatus {
    /// The status of a channel that has not connected yet.
    pub fn offline() -> Self {
        Self::default()
    }

    /// The status of a channel that is up.
    pub fn online_at(now: &str) -> Self {
        Self {
            online: true,
            last_handshake_at: Some(now.to_string()),
            last_error: None,
            ..Self::default()
        }
    }

    /// The status of a channel that failed.
    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            online: false,
            last_handshake_at: None,
            last_error: Some(error.into()),
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_status_is_offline_without_an_error() {
        let status = DashboardStatus::offline();
        assert!(!status.online);
        assert!(status.last_handshake_at.is_none());
        assert!(status.last_error.is_none());
    }

    #[test]
    fn an_online_status_carries_the_handshake_time() {
        let status = DashboardStatus::online_at("2026-11-25T10:00:00Z");
        assert!(status.online);
        assert_eq!(
            status.last_handshake_at.as_deref(),
            Some("2026-11-25T10:00:00Z")
        );
        assert!(status.last_error.is_none());
    }

    #[test]
    fn a_failed_status_carries_the_error() {
        let status = DashboardStatus::failed("the dashboard refused the connection");
        assert!(!status.online);
        assert_eq!(
            status.last_error.as_deref(),
            Some("the dashboard refused the connection")
        );
    }
}
