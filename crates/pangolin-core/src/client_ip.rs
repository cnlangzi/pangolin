//! Resolve the real client IP given a [`SystemConfig`], the TCP
//! peer address (when available), and a header-lookup closure.
//!
//! Pure function: no I/O, no async. Lives in `pangolin-core` so both
//! `ngx` (`record_access_log` + tunnel frame builders) and the admin
//! route handler can call it, and so the unit tests in this file run
//! in milliseconds without dragging in pingora.
//!
//! ## Why a closure returning owned strings?
//!
//! `pangolin_core` deliberately avoids a `pingora` dependency. A
//! closure returning `Option<&str>` ties the lifetime of the
//! return value to the closure's captures (e.g. `session`), which
//! the `Fn` trait cannot express for the cross-context lifetime
//! span the ngx call site needs. We accept `Option<String>` (owned)
//! instead — the per-request allocation is one short string
//! (an IPv4 address is <16 bytes), well below the cost of the
//! surrounding pingora/reqwest machinery, and the test fixtures
//! pass `&'static str` literals via `.to_string()` with no extra
//! allocation pressure.
//!
//! ## Resolution rules
//!
//! Per [`FrontendMode`]:
//!
//! * `Direct` — return `peer.ip()` (port stripped). If peer is
//!   unavailable, return [`UNKNOWN`].
//! * `Cloudflare` — return the value of the `CF-Connecting-IP`
//!   header. If the header is missing or empty, fall back to
//!   `peer.ip()`. If both are unavailable, return [`UNKNOWN`].
//! * `CustomLb` — for each header name in `cfg.trusted_headers`
//!   (or [`DEFAULT_CUSTOM_HEADERS`] if the list is empty), return
//!   the first present + non-empty value. If none match, fall
//!   back to `peer.ip()`. If both are unavailable, return
//!   [`UNKNOWN`].
//!
//! The "fall back to peer" behaviour (rather than logging an empty
//! value) is deliberate: an operator looking at an access log row
//! for a request that should have been fronted by Cloudflare will
//! find a hop IP that they can correlate against CDN logs, instead
//! of a useless blank.

use std::net::SocketAddr;

use crate::types::{FrontendMode, SystemConfig};

/// Default header list consulted when `frontend_mode = CustomLb`
/// and `cfg.trusted_headers` is empty.
pub const DEFAULT_CUSTOM_HEADERS: &[&str] = &["X-Real-IP"];

/// Final-fallback value when neither a configured header nor a
/// peer address is available. Matches the pre-fix behaviour in
/// `ngx/src/proxy.rs::record_access_log`, so old log-parsing scripts
/// do not break.
pub const UNKNOWN: &str = "unknown";

/// Resolve the visitor IP that should appear in the access log.
///
/// See the module-level doc comment for the per-mode rules and
/// the rationale for the owned-string header lookup.
pub fn resolve<F>(cfg: &SystemConfig, peer: Option<SocketAddr>, get_header: F) -> String
where
    F: Fn(&str) -> Option<String>,
{
    let peer_ip = peer.map(|a| a.ip());

    match cfg.frontend_mode {
        FrontendMode::Direct => peer_ip_to_string(peer_ip),
        FrontendMode::Cloudflare => first_non_empty(&get_header, &["CF-Connecting-IP"])
            .unwrap_or_else(|| peer_ip_to_string(peer_ip)),
        FrontendMode::CustomLb => {
            let names: Vec<&str> = if cfg.trusted_headers.is_empty() {
                DEFAULT_CUSTOM_HEADERS.to_vec()
            } else {
                cfg.trusted_headers.iter().map(String::as_str).collect()
            };
            first_non_empty(&get_header, &names).unwrap_or_else(|| peer_ip_to_string(peer_ip))
        }
    }
}

/// Iterate over `names` (in order) and return the first present +
/// non-empty header value. Returns `None` when no name matches.
fn first_non_empty<F>(get_header: &F, names: &[&str]) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    for name in names {
        if let Some(v) = get_header(name)
            && let Some(trimmed) = non_empty_or_none(&v)
        {
            return Some(trimmed.to_owned());
        }
    }
    None
}

/// Trim the value, then return it only if non-empty.
fn non_empty_or_none(s: &str) -> Option<&str> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Render an `Option<IpAddr>` as a string, with [`UNKNOWN`] as
/// the fallback when the value is `None`.
fn peer_ip_to_string(ip: Option<std::net::IpAddr>) -> String {
    ip.map(|i| i.to_string())
        .unwrap_or_else(|| UNKNOWN.to_string())
}

/// Parse the `trusted_headers` column value (a JSON-encoded array
/// stored as TEXT) into a `Vec<String>`.
///
/// An empty string or `"[]"` both normalise to an empty `Vec`,
/// which the resolver then expands to [`DEFAULT_CUSTOM_HEADERS`].
/// Malformed JSON also returns an empty `Vec` — the admin POST
/// handler validates the JSON before writing, so this is purely
/// defensive (e.g. for hand-edited rows).
pub fn parse_trusted_headers(raw: &str) -> Vec<String> {
    if raw.is_empty() {
        return Vec::new();
    }
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SystemConfig;
    use chrono::Utc;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn cfg(mode: FrontendMode, headers: Vec<&str>) -> SystemConfig {
        SystemConfig {
            frontend_mode: mode,
            trusted_headers: headers.into_iter().map(String::from).collect(),
            updated_at: Utc::now(),
        }
    }

    fn peer(a: u8, b: u8, c: u8, d: u8) -> Option<SocketAddr> {
        Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(a, b, c, d)), 0))
    }

    fn no_peer() -> Option<SocketAddr> {
        None
    }

    fn headers(pairs: Vec<(&'static str, &'static str)>) -> impl Fn(&str) -> Option<String> {
        move |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn direct_strips_port() {
        // peer `1.2.3.4:5678` → "1.2.3.4"
        let peer = Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 5678));
        assert_eq!(
            resolve(&cfg(FrontendMode::Direct, vec![]), peer, headers(vec![])),
            "1.2.3.4"
        );
    }

    #[test]
    fn direct_missing_peer_returns_unknown() {
        assert_eq!(
            resolve(
                &cfg(FrontendMode::Direct, vec![]),
                no_peer(),
                headers(vec![])
            ),
            UNKNOWN
        );
    }

    #[test]
    fn cloudflare_wins_over_peer() {
        // Even when the peer is set, CF-Connecting-IP takes precedence.
        assert_eq!(
            resolve(
                &cfg(FrontendMode::Cloudflare, vec![]),
                peer(1, 2, 3, 4),
                headers(vec![("CF-Connecting-IP", "203.0.113.7")])
            ),
            "203.0.113.7"
        );
    }

    #[test]
    fn cloudflare_missing_falls_back_to_peer() {
        assert_eq!(
            resolve(
                &cfg(FrontendMode::Cloudflare, vec![]),
                peer(1, 2, 3, 4),
                headers(vec![])
            ),
            "1.2.3.4"
        );
    }

    #[test]
    fn cloudflare_empty_value_falls_back_to_peer() {
        // An empty header value is treated as missing.
        assert_eq!(
            resolve(
                &cfg(FrontendMode::Cloudflare, vec![]),
                peer(1, 2, 3, 4),
                headers(vec![("CF-Connecting-IP", "")])
            ),
            "1.2.3.4"
        );
    }

    #[test]
    fn cloudflare_whitespace_value_falls_back_to_peer() {
        // Whitespace-only is also treated as missing (trim).
        assert_eq!(
            resolve(
                &cfg(FrontendMode::Cloudflare, vec![]),
                peer(1, 2, 3, 4),
                headers(vec![("CF-Connecting-IP", "   ")])
            ),
            "1.2.3.4"
        );
    }

    #[test]
    fn cloudflare_missing_peer_returns_unknown() {
        assert_eq!(
            resolve(
                &cfg(FrontendMode::Cloudflare, vec![]),
                no_peer(),
                headers(vec![])
            ),
            UNKNOWN
        );
    }

    #[test]
    fn custom_lb_priority_order() {
        // The first header in the list wins when both are present.
        assert_eq!(
            resolve(
                &cfg(
                    FrontendMode::CustomLb,
                    vec!["X-Real-IP", "CF-Connecting-IP"]
                ),
                peer(1, 2, 3, 4),
                headers(vec![
                    ("X-Real-IP", "10.0.0.1"),
                    ("CF-Connecting-IP", "203.0.113.7"),
                ])
            ),
            "10.0.0.1"
        );
    }

    #[test]
    fn custom_lb_first_present_wins() {
        // Only the second is present; the first is missing.
        assert_eq!(
            resolve(
                &cfg(FrontendMode::CustomLb, vec!["X-Real-IP", "X-Client-IP"]),
                peer(1, 2, 3, 4),
                headers(vec![("X-Client-IP", "10.0.0.2")])
            ),
            "10.0.0.2"
        );
    }

    #[test]
    fn custom_lb_none_match_falls_back_to_peer() {
        assert_eq!(
            resolve(
                &cfg(FrontendMode::CustomLb, vec!["X-Real-IP"]),
                peer(1, 2, 3, 4),
                headers(vec![])
            ),
            "1.2.3.4"
        );
    }

    #[test]
    fn custom_lb_empty_list_uses_default() {
        // An empty `trusted_headers` falls back to DEFAULT_CUSTOM_HEADERS.
        assert_eq!(
            resolve(
                &cfg(FrontendMode::CustomLb, vec![]),
                peer(1, 2, 3, 4),
                headers(vec![("X-Real-IP", "10.0.0.5")])
            ),
            "10.0.0.5"
        );
    }

    #[test]
    fn custom_lb_missing_peer_returns_unknown() {
        assert_eq!(
            resolve(
                &cfg(FrontendMode::CustomLb, vec!["X-Real-IP"]),
                no_peer(),
                headers(vec![])
            ),
            UNKNOWN
        );
    }

    #[test]
    fn parse_trusted_headers_handles_empty_string() {
        assert!(parse_trusted_headers("").is_empty());
    }

    #[test]
    fn parse_trusted_headers_handles_bad_json() {
        // Defensive: a malformed row shouldn't crash the resolver;
        // the admin POST handler is the gatekeeper.
        assert!(parse_trusted_headers("not-json").is_empty());
        assert!(parse_trusted_headers("{").is_empty());
    }

    #[test]
    fn parse_trusted_headers_round_trip() {
        let raw = r#"["X-Real-IP","X-Forwarded-For"]"#;
        assert_eq!(
            parse_trusted_headers(raw),
            vec!["X-Real-IP".to_string(), "X-Forwarded-For".to_string()]
        );
    }

    #[test]
    fn unknown_constant_matches_record_access_log_fallback() {
        // Pin the literal so a future rename of the placeholder
        // forces an update here (and reviewers notice).
        assert_eq!(UNKNOWN, "unknown");
    }
}
