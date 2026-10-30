//! Resolve the real client IP given a [`SystemConfig`], the TCP
//! peer address (when available), and a header-lookup closure.
//!
//! Pure function: no I/O, no async. Lives in `pangolin-core` so both
//! `ngx` (`record_access_log` + the tunnel frame builders) and the
//! admin route handler can call it, and so the unit tests in this file
//! run in milliseconds without dragging in pingora.
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
//! ## The two notions of "client IP"
//!
//! * **Authoritative client IP** ([`resolve_authoritative`]) — the
//!   visitor's address as reported by a *trusted* source: the TCP
//!   peer in `direct` mode, the CDN / LB header in `cloudflare` /
//!   `custom_lb` mode. Never a proxy hop. `None` when there is no
//!   such source.
//! * **Log client IP** ([`resolve`]) — the authoritative client IP
//!   when there is one, otherwise the TCP peer, otherwise the
//!   literal [`UNKNOWN`]. The peer fallback is deliberate: an
//!   operator looking at an access log row for a request that should
//!   have been fronted by Cloudflare finds a hop IP they can
//!   correlate against CDN logs, instead of a useless blank.
//!
//! The forwarded `P-Client-Ip` header uses the *authoritative* IP
//! (empty when there is none); the access log uses the *log* IP.
//! Both come from the same single resolution per request — see
//! `docs/design/gateway-headers.md`.
//!
//! ## Resolution rules
//!
//! Per [`FrontendMode`]:
//!
//! * `Direct` — the TCP peer (normalized). No header is consulted.
//! * `Cloudflare` — the Cloudflare-derived address: the value of
//!   `CF-Connecting-IP`, unless it is a Cloudflare *Pseudo IPv4*
//!   address and `CF-Connecting-IPv6` carries a parseable IPv6, in
//!   which case the IPv6 wins (see [`resolve_authoritative`]).
//! * `CustomLb` — the value of `cfg.client_ip_header`, normalized.
//!   An unset key, an absent header and an unparseable value all
//!   mean "no authoritative client IP".
//!
//! Every value is normalized ([`normalize_ip`]): an IPv4-mapped IPv6
//! address is unwrapped to its IPv4 form and a port is stripped, so
//! the access log, the bot verification and the forwarded header all
//! show the same string for the same visitor.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use crate::types::{FrontendMode, SystemConfig};

/// Final-fallback value when neither a configured header nor a
/// peer address is available. Matches the pre-fix behaviour in
/// `ngx/src/proxy.rs::record_access_log`, so old log-parsing scripts
/// do not break.
pub const UNKNOWN: &str = "unknown";

/// Cloudflare's Pseudo IPv4 feature (in "Overwrite Headers" mode)
/// replaces the visitor's IPv6 with a synthetic Class E address in
/// `240.0.0.0/4`. See <https://developers.cloudflare.com/fundamentals/reference/pseudo-ipv4/>.
const PSEUDO_IPV4_FIRST_OCTET: u8 = 240;

/// Resolve the visitor IP that should appear in the access log.
///
/// The one-shot form of the resolution: the authoritative client IP
/// ([`resolve_authoritative`]) with the peer fallback and the
/// `unknown` placeholder applied ([`log_value`]).
///
/// The proxy hot path does **not** use this function — it needs the
/// authoritative IP on its own (for the `P-Client-Ip` header and the
/// bot verification) and therefore calls the two functions directly
/// so the client IP is resolved exactly once per request. Use this
/// entry point when only the log value is wanted.
pub fn resolve<F>(cfg: &SystemConfig, peer: Option<SocketAddr>, get_header: F) -> String
where
    F: Fn(&str) -> Option<String>,
{
    log_value(resolve_authoritative(cfg, peer, &get_header), peer)
}

/// The access-log `client_ip` value for an already-resolved
/// authoritative client IP.
///
/// The authoritative IP when there is one, otherwise the TCP peer,
/// otherwise [`UNKNOWN`]. Callers that already resolved the
/// authoritative IP (the proxy hot path resolves once per request)
/// use this instead of resolving a second time.
pub fn log_value(authoritative: Option<IpAddr>, peer: Option<SocketAddr>) -> String {
    authoritative
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| peer_ip_string(peer))
}

/// The authoritative client IP: the visitor's address as reported by
/// a trusted source. `None` when there is none.
///
/// Unlike [`resolve`] this **never** falls back to the TCP peer
/// outside `direct` mode: behind a CDN / LB the peer is a proxy
/// hop, and putting it in `P-Client-Ip` would be wrong. The access
/// log still falls back to the peer (see [`resolve`]) so operators
/// can correlate with CDN logs.
pub fn resolve_authoritative<F>(
    cfg: &SystemConfig,
    peer: Option<SocketAddr>,
    get_header: F,
) -> Option<IpAddr>
where
    F: Fn(&str) -> Option<String>,
{
    match cfg.frontend_mode {
        // The TCP peer *is* the visitor in direct mode.
        FrontendMode::Direct => peer.map(|a| normalize_addr(a.ip())),
        FrontendMode::Cloudflare => cloudflare_client_ip(&get_header),
        FrontendMode::CustomLb => match cfg.client_ip_header.as_deref() {
            Some(name) => get_header(name).and_then(|v| normalize_ip(&v)),
            None => None,
        },
    }
}

/// Normalize a raw header / peer value into an IP address.
///
/// * `::ffff:203.0.113.7` → `203.0.113.7` (IPv4-mapped IPv6,
///   RFC 4291 §2.5.5.1 — the embedded IPv4 is the real address)
/// * `203.0.113.7:54321` → `203.0.113.7` (IPv4 with a port)
/// * `[2001:db8::1]:443` → `2001:db8::1` (IPv6 with a port)
/// * `not-an-ip` → `None`
///
/// A bare IPv6 address is never split on `:` — `2001:db8::1` is a
/// valid address and the port is ambiguous, so the value is parsed
/// as-is. Brackets are the only unambiguous port delimiter for
/// IPv6.
pub fn normalize_ip(raw: &str) -> Option<IpAddr> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    // Bracketed IPv6, optionally with a port: `[2001:db8::1]:443`.
    if let Some(rest) = s.strip_prefix('[') {
        let (inner, tail) = rest.split_once(']')?;
        if !(tail.is_empty() || tail.starts_with(':')) {
            return None;
        }
        return parse_ip(inner);
    }
    // More than one colon → a bare IPv6 address (never split it).
    if s.matches(':').count() > 1 {
        return parse_ip(s);
    }
    // Exactly one colon → `ipv4:port`.
    if let Some((addr, _port)) = s.rsplit_once(':') {
        return parse_ip(addr);
    }
    parse_ip(s)
}

/// Parse a bare address (no port) and unwrap IPv4-mapped IPv6.
fn parse_ip(s: &str) -> Option<IpAddr> {
    s.parse::<IpAddr>().ok().map(normalize_addr)
}

/// Unwrap an IPv4-mapped IPv6 address (`::ffff:a.b.c.d`) to its IPv4
/// form.
///
/// A dual-stack listener reports IPv4 clients as `::ffff:a.b.c.d`.
/// Left un-normalized, one visitor splits into two log rows and —
/// worse — never matches an IPv4 bot CIDR (`IpNet::contains` does not
/// match across address families), so verified crawlers were logged
/// as unverified.
pub fn normalize_addr(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        other => other,
    }
}

/// Render the TCP peer as a bare IP string, or [`UNKNOWN`] when the
/// peer address is unavailable.
fn peer_ip_string(peer: Option<SocketAddr>) -> String {
    peer.map(|a| normalize_addr(a.ip()))
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| UNKNOWN.to_string())
}

/// The Cloudflare-derived client address.
///
/// 1. `CF-Connecting-IP` and `CF-Connecting-IPv6` are read and
///    normalized independently.
/// 2. If `CF-Connecting-IP` is an IPv4 address in `240.0.0.0/4`
///    (Cloudflare's Pseudo IPv4) **and** a parseable
///    `CF-Connecting-IPv6` exists, the IPv6 wins — the IPv4 is
///    synthetic.
/// 3. Otherwise `CF-Connecting-IP` wins; if it is absent,
///    `CF-Connecting-IPv6` is used.
fn cloudflare_client_ip<F>(get_header: &F) -> Option<IpAddr>
where
    F: Fn(&str) -> Option<String>,
{
    let v4 = get_header("CF-Connecting-IP").and_then(|v| normalize_ip(&v));
    let v6 = get_header("CF-Connecting-IPv6").and_then(|v| normalize_ip(&v));

    if let Some(IpAddr::V4(v4_addr)) = v4 {
        if is_pseudo_ipv4(v4_addr) && v6.is_some() {
            return v6;
        }
        return Some(IpAddr::V4(v4_addr));
    }
    v6.or(v4)
}

/// True for Cloudflare's Pseudo IPv4 range (`240.0.0.0/4`, the
/// Class E "reserved" block).
fn is_pseudo_ipv4(addr: Ipv4Addr) -> bool {
    addr.octets()[0] >= PSEUDO_IPV4_FIRST_OCTET
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SystemConfig;
    use chrono::Utc;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn cfg(mode: FrontendMode) -> SystemConfig {
        SystemConfig {
            frontend_mode: mode,
            pgl_headers_enabled: true,
            client_ip_header: None,
            client_country_header: None,
            client_region_header: None,
            updated_at: Utc::now(),
        }
    }

    fn cfg_custom_lb(ip_header: Option<&str>) -> SystemConfig {
        let mut c = cfg(FrontendMode::CustomLb);
        c.client_ip_header = ip_header.map(String::from);
        c
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

    // ── normalize_ip ────────────────────────────────────────────

    #[test]
    fn normalize_ip_unwraps_ipv4_mapped_ipv6() {
        assert_eq!(
            normalize_ip("::ffff:203.0.113.7").map(|ip| ip.to_string()),
            Some("203.0.113.7".to_string())
        );
    }

    #[test]
    fn normalize_ip_strips_ipv4_port() {
        assert_eq!(
            normalize_ip("203.0.113.7:54321").map(|ip| ip.to_string()),
            Some("203.0.113.7".to_string())
        );
    }

    #[test]
    fn normalize_ip_strips_bracketed_ipv6_port() {
        assert_eq!(
            normalize_ip("[2001:db8::1]:443").map(|ip| ip.to_string()),
            Some("2001:db8::1".to_string())
        );
    }

    #[test]
    fn normalize_ip_keeps_bare_ipv6() {
        assert_eq!(
            normalize_ip("2001:db8::1").map(|ip| ip.to_string()),
            Some("2001:db8::1".to_string())
        );
    }

    #[test]
    fn normalize_ip_rejects_garbage() {
        assert!(normalize_ip("not-an-ip").is_none());
        assert!(normalize_ip("").is_none());
        assert!(normalize_ip("   ").is_none());
    }

    // ── resolve (access log) ────────────────────────────────────

    #[test]
    fn direct_strips_port() {
        // peer `1.2.3.4:5678` → "1.2.3.4"
        let peer = Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 5678));
        assert_eq!(
            resolve(&cfg(FrontendMode::Direct), peer, headers(vec![])),
            "1.2.3.4"
        );
    }

    #[test]
    fn direct_missing_peer_returns_unknown() {
        assert_eq!(
            resolve(&cfg(FrontendMode::Direct), no_peer(), headers(vec![])),
            UNKNOWN
        );
    }

    #[test]
    fn cloudflare_wins_over_peer() {
        // Even when the peer is set, CF-Connecting-IP takes precedence.
        assert_eq!(
            resolve(
                &cfg(FrontendMode::Cloudflare),
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
                &cfg(FrontendMode::Cloudflare),
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
                &cfg(FrontendMode::Cloudflare),
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
                &cfg(FrontendMode::Cloudflare),
                peer(1, 2, 3, 4),
                headers(vec![("CF-Connecting-IP", "   ")])
            ),
            "1.2.3.4"
        );
    }

    #[test]
    fn cloudflare_missing_peer_returns_unknown() {
        assert_eq!(
            resolve(&cfg(FrontendMode::Cloudflare), no_peer(), headers(vec![])),
            UNKNOWN
        );
    }

    #[test]
    fn cloudflare_unparseable_value_falls_back_to_peer() {
        // A first-level header that is not an IP is treated as absent
        // (the access log must never carry a non-IP string).
        assert_eq!(
            resolve(
                &cfg(FrontendMode::Cloudflare),
                peer(1, 2, 3, 4),
                headers(vec![("CF-Connecting-IP", "not-an-ip")])
            ),
            "1.2.3.4"
        );
    }

    #[test]
    fn cloudflare_ipv4_mapped_value_is_unwrapped() {
        assert_eq!(
            resolve(
                &cfg(FrontendMode::Cloudflare),
                peer(1, 2, 3, 4),
                headers(vec![("CF-Connecting-IP", "::ffff:203.0.113.7")])
            ),
            "203.0.113.7"
        );
    }

    #[test]
    fn custom_lb_uses_the_configured_key() {
        assert_eq!(
            resolve(
                &cfg_custom_lb(Some("X-Client-IP")),
                peer(1, 2, 3, 4),
                headers(vec![("X-Client-IP", "198.51.100.9")])
            ),
            "198.51.100.9"
        );
    }

    #[test]
    fn custom_lb_unset_key_falls_back_to_peer() {
        assert_eq!(
            resolve(&cfg_custom_lb(None), peer(1, 2, 3, 4), headers(vec![])),
            "1.2.3.4"
        );
    }

    #[test]
    fn custom_lb_absent_header_falls_back_to_peer() {
        assert_eq!(
            resolve(
                &cfg_custom_lb(Some("X-Client-IP")),
                peer(1, 2, 3, 4),
                headers(vec![("X-Other-IP", "198.51.100.9")])
            ),
            "1.2.3.4"
        );
    }

    #[test]
    fn custom_lb_unparseable_value_falls_back_to_peer() {
        assert_eq!(
            resolve(
                &cfg_custom_lb(Some("X-Client-IP")),
                peer(1, 2, 3, 4),
                headers(vec![("X-Client-IP", "not-an-ip")])
            ),
            "1.2.3.4"
        );
    }

    #[test]
    fn custom_lb_missing_peer_returns_unknown() {
        assert_eq!(
            resolve(
                &cfg_custom_lb(Some("X-Client-IP")),
                no_peer(),
                headers(vec![])
            ),
            UNKNOWN
        );
    }

    #[test]
    fn peer_ipv4_mapped_is_normalized() {
        // A dual-stack listener reports IPv4 clients as
        // `::ffff:a.b.c.d`; both the log and the header must show
        // the plain IPv4 form.
        let peer = Some(SocketAddr::new(
            IpAddr::V6("::ffff:127.0.0.1".parse::<std::net::Ipv6Addr>().unwrap()),
            5555,
        ));
        assert_eq!(
            resolve(&cfg(FrontendMode::Direct), peer, headers(vec![])),
            "127.0.0.1"
        );
    }

    // ── resolve_authoritative ───────────────────────────────────

    #[test]
    fn authoritative_direct_is_the_peer() {
        assert_eq!(
            resolve_authoritative(
                &cfg(FrontendMode::Direct),
                peer(1, 2, 3, 4),
                headers(vec![])
            ),
            Some("1.2.3.4".parse::<IpAddr>().unwrap())
        );
    }

    #[test]
    fn authoritative_direct_ignores_headers() {
        // A `CF-IPCountry`-style header sent by the client must not
        // influence the client IP in direct mode.
        assert_eq!(
            resolve_authoritative(
                &cfg(FrontendMode::Direct),
                peer(1, 2, 3, 4),
                headers(vec![("CF-Connecting-IP", "203.0.113.7")])
            ),
            Some("1.2.3.4".parse::<IpAddr>().unwrap())
        );
    }

    #[test]
    fn authoritative_cloudflare_without_header_is_none() {
        // No peer fallback outside direct mode: the peer is a CDN
        // hop, not the visitor.
        assert_eq!(
            resolve_authoritative(
                &cfg(FrontendMode::Cloudflare),
                peer(1, 2, 3, 4),
                headers(vec![])
            ),
            None
        );
    }

    #[test]
    fn authoritative_cloudflare_prefers_ipv6_when_ipv4_is_pseudo() {
        // Cloudflare Pseudo IPv4: the IPv4 is synthetic (Class E),
        // the real address is in CF-Connecting-IPv6.
        assert_eq!(
            resolve_authoritative(
                &cfg(FrontendMode::Cloudflare),
                peer(1, 2, 3, 4),
                headers(vec![
                    ("CF-Connecting-IP", "243.201.173.220"),
                    ("CF-Connecting-IPv6", "2001:db8::1"),
                ])
            ),
            Some("2001:db8::1".parse::<IpAddr>().unwrap())
        );
    }

    #[test]
    fn authoritative_cloudflare_keeps_normal_ipv4() {
        assert_eq!(
            resolve_authoritative(
                &cfg(FrontendMode::Cloudflare),
                peer(1, 2, 3, 4),
                headers(vec![("CF-Connecting-IP", "203.0.113.7")])
            ),
            Some("203.0.113.7".parse::<IpAddr>().unwrap())
        );
    }

    #[test]
    fn authoritative_cloudflare_pseudo_ipv4_without_ipv6_keeps_ipv4() {
        // No parseable CF-Connecting-IPv6 → the (synthetic) IPv4 is
        // all we have.
        assert_eq!(
            resolve_authoritative(
                &cfg(FrontendMode::Cloudflare),
                peer(1, 2, 3, 4),
                headers(vec![("CF-Connecting-IP", "243.201.173.220")])
            ),
            Some("243.201.173.220".parse::<IpAddr>().unwrap())
        );
    }

    #[test]
    fn authoritative_cloudflare_falls_back_to_ipv6_header() {
        assert_eq!(
            resolve_authoritative(
                &cfg(FrontendMode::Cloudflare),
                peer(1, 2, 3, 4),
                headers(vec![("CF-Connecting-IPv6", "2001:db8::1")])
            ),
            Some("2001:db8::1".parse::<IpAddr>().unwrap())
        );
    }

    #[test]
    fn authoritative_custom_lb_without_key_is_none() {
        assert_eq!(
            resolve_authoritative(
                &cfg_custom_lb(None),
                peer(1, 2, 3, 4),
                headers(vec![("X-Client-IP", "198.51.100.9")])
            ),
            None
        );
    }

    #[test]
    fn unknown_constant_matches_record_access_log_fallback() {
        // Pin the literal so a future rename of the placeholder
        // forces an update here (and reviewers notice).
        assert_eq!(UNKNOWN, "unknown");
    }
}
