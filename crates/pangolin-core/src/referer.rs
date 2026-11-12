//! Referer parsing and referral-eligibility rules.
//!
//! Three pure functions, no I/O, so the whole detection rule is
//! unit-testable without a running gateway:
//!
//! 1. [`parse_referer`] — extract the referer host from the raw
//!    `Referer` header.
//! 2. [`is_document_request`] — decide whether the request is a
//!    top-level page load (as opposed to an asset / XHR / iframe).
//! 3. [`is_external_referer`] — decide whether the referer belongs
//!    to a *different* site than the one serving the request.
//!
//! The three are deliberately separate: the caller resolves the
//! current site and the domain index once (inside the index lock it
//! already holds) and then applies them in order.
//!
//! ## Why the site boundary, not the hostname
//!
//! The boundary the operator cares about is the *site*, not the
//! hostname. A link from `shop.example.com` to `www.example.com` is
//! internal to the site `web` and must not appear in the site's
//! inbound-external-referral analysis. Conversely a link from one of
//! our sites to *another* of our sites is recorded as an ordinary
//! external referral — the operator explicitly does not want it
//! labelled as ours.
//!
//! See `docs/design/referer-log.md` for the full design.

use std::collections::HashMap;
use std::sync::Arc;

use crate::index::site_of_host;
use crate::types::Site;

/// The outcome of parsing a `Referer` header.
///
/// Both fields are always present on a successful parse:
/// - `referer` — the URL with its query string, fragment and
///   userinfo removed. The scheme, the original host text (including
///   any port) and the path are kept verbatim, so the operator can
///   still see which linking page sent the traffic.
/// - `referer_host` — the lowercased hostname with the port and the
///   userinfo stripped. This is the grouping key for the statistics
///   rollup and the key the site-boundary check runs against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefererHit {
    /// Referer URL without userinfo, query string and fragment.
    pub referer: String,
    /// Lowercased hostname of the referer, without port.
    pub referer_host: String,
}

/// Extract the host from a `Referer` header value.
///
/// Returns `None` when the value carries no attributable host:
///
/// - empty or whitespace-only,
/// - not an absolute URL (`/relative/path`, `about:blank`,
///   `data:…`, `javascript:…` — all lack a `//` authority),
/// - an authority with no host (`https://`).
///
/// The stored URL keeps the scheme, the original host text and the
/// path; the query string, the fragment and the userinfo are dropped.
/// Tracking parameters and referral tokens live in the query string,
/// and the linking page's path is what the SEO analysis needs.
pub fn parse_referer(raw: &str) -> Option<RefererHit> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    // An absolute URL with an authority is `scheme://authority/path`.
    // Requiring the `://` is what rejects `about:blank` and
    // `/relative/path` without a scheme-specific allow-list.
    let (scheme, rest) = raw.split_once("://")?;
    if !is_valid_scheme(scheme) {
        return None;
    }

    // The authority ends at the first `/`, `?` or `#`.
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let after_authority = &rest[authority_end..];

    // Drop the userinfo (`User:pw@`). The stored URL must not carry
    // credentials, and the host must not either.
    let host_port = match authority.rsplit_once('@') {
        Some((_userinfo, host_port)) => host_port,
        None => authority,
    };
    if host_port.is_empty() {
        return None;
    }

    let host = strip_port(host_port);
    if host.is_empty() {
        return None;
    }

    // The path stops at the query string / fragment.
    let path = match after_authority.find(['?', '#']) {
        Some(idx) => &after_authority[..idx],
        None => after_authority,
    };

    Some(RefererHit {
        referer: format!("{scheme}://{host_port}{path}"),
        referer_host: host.to_lowercase(),
    })
}

/// A scheme is `ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )` per RFC
/// 3986. Anything else is not a URL we should attribute a host to.
fn is_valid_scheme(scheme: &str) -> bool {
    let mut chars = scheme.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
}

/// Strip the port from an authority's host portion.
///
/// Handles the bracketed IPv6 literal form (`[::1]:8443`) and a
/// trailing `:port`. A colon that is not followed by digits is kept
/// as part of the host — it is not a port, and dropping it would
/// silently merge two different authorities.
fn strip_port(host_port: &str) -> &str {
    if let Some(bracket_end) = host_port.find(']') {
        // IPv6 literal: keep `[...]` and drop whatever follows.
        return &host_port[..=bracket_end];
    }
    match host_port.rsplit_once(':') {
        Some((host, port)) if port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => host_port,
    }
}

/// Decide whether a request is a top-level document navigation.
///
/// `Sec-Fetch-Dest` wins whenever it is present: it distinguishes a
/// top-level navigation from a `fetch`/XHR issued against the very
/// same path, which `Accept` cannot. The `Accept` fallback applies
/// only to clients that omit `Sec-Fetch-Dest` entirely (older
/// browsers, `curl`, some crawlers).
///
/// A single page view pulls a couple of dozen subresources, each of
/// which carries the same `Referer` header — counting requests would
/// inflate the traffic figure roughly twentyfold. Only document
/// navigations are eligible.
pub fn is_document_request(sec_fetch_dest: Option<&str>, accept: Option<&str>) -> bool {
    match sec_fetch_dest {
        Some(dest) => dest.trim().eq_ignore_ascii_case("document"),
        None => accept.is_some_and(|a| a.contains("text/html")),
    }
}

/// Decide whether a referral is external to `current_site`.
///
/// A referral is external when the referer host belongs to a
/// *different* site of ours — or to no site of ours at all. A
/// referral whose owning site equals `current_site` is internal and
/// is not recorded.
///
/// `domain` is the router's domain index (`Indexes.domain`): exact
/// FQDNs and wildcard literals. The resolution goes through
/// [`site_of_host`] so the request-time decision and the query-time
/// grouping use one rule.
pub fn is_external_referer(
    referer_host: &str,
    current_site: &str,
    domain: &HashMap<String, Arc<Site>>,
) -> bool {
    match site_of_host(domain, referer_host) {
        Some(site) => site.name != current_site,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Indexes;
    use crate::types::{Domain, HostMode, Site};
    use chrono::Utc;

    fn site(name: &str) -> Site {
        let now = Utc::now();
        Site {
            name: name.into(),
            backend: "http://127.0.0.1:8080".into(),
            enabled: true,
            created_at: now,
            updated_at: now,
            host_mode: HostMode::Passthrough,
            host_custom: None,
            domain_count: 0,
        }
    }

    fn domain(domain: &str, site_name: &str) -> Domain {
        let now = Utc::now();
        Domain {
            domain: domain.into(),
            site_name: site_name.into(),
            enabled: true,
            auto_issue: false,
            dns_provider: None,
            challenge_kind: None,
            created_at: now,
        }
    }

    /// Domain index for a site owning `example.com` + `www.example.com`.
    fn web_index() -> Indexes {
        Indexes::build(
            vec![site("web")],
            vec![
                domain("example.com", "web"),
                domain("www.example.com", "web"),
            ],
        )
    }

    // ── parse_referer ───────────────────────────────────────────

    #[test]
    fn parse_referer_extracts_host_and_strips_query() {
        let hit = parse_referer("https://news.example.org/article?id=7").unwrap();
        assert_eq!(hit.referer_host, "news.example.org");
        assert_eq!(hit.referer, "https://news.example.org/article");
    }

    #[test]
    fn parse_referer_strips_userinfo_port_and_fragment() {
        let hit = parse_referer("https://User:pw@News.Example.ORG:8443/a/b?q=1#frag").unwrap();
        assert_eq!(hit.referer_host, "news.example.org");
        assert_eq!(hit.referer, "https://News.Example.ORG:8443/a/b");
    }

    #[test]
    fn parse_referer_keeps_path_only_url() {
        let hit = parse_referer("http://example.com").unwrap();
        assert_eq!(hit.referer_host, "example.com");
        assert_eq!(hit.referer, "http://example.com");
    }

    #[test]
    fn parse_referer_keeps_bare_root_path() {
        let hit = parse_referer("https://example.com/").unwrap();
        assert_eq!(hit.referer, "https://example.com/");
    }

    #[test]
    fn parse_referer_accepts_non_http_schemes_with_authority() {
        let hit = parse_referer("ftp://files.example.com/pub").unwrap();
        assert_eq!(hit.referer_host, "files.example.com");
        assert_eq!(hit.referer, "ftp://files.example.com/pub");
    }

    #[test]
    fn parse_referer_rejects_empty_and_blank() {
        assert!(parse_referer("").is_none());
        assert!(parse_referer("   ").is_none());
        assert!(parse_referer("\t\n").is_none());
    }

    #[test]
    fn parse_referer_rejects_relative_and_opaque_urls() {
        assert!(parse_referer("/relative/path").is_none());
        assert!(parse_referer("about:blank").is_none());
        assert!(parse_referer("data:text/html;base64,AAAA").is_none());
        assert!(parse_referer("javascript:void(0)").is_none());
    }

    #[test]
    fn parse_referer_rejects_authority_without_host() {
        assert!(parse_referer("https://").is_none());
        assert!(parse_referer("https:///path").is_none());
        assert!(parse_referer("https://user@").is_none());
    }

    #[test]
    fn parse_referer_rejects_bad_scheme() {
        assert!(parse_referer("1http://example.com").is_none());
        assert!(parse_referer("://example.com").is_none());
    }

    #[test]
    fn parse_referer_lowercases_host_only() {
        let hit = parse_referer("HTTPS://EXAMPLE.COM/Path").unwrap();
        assert_eq!(hit.referer_host, "example.com");
        // The scheme and path keep their original text; only the
        // host is lowercased for the grouping key.
        assert_eq!(hit.referer, "HTTPS://EXAMPLE.COM/Path");
    }

    // ── is_document_request ─────────────────────────────────────

    #[test]
    fn sec_fetch_dest_document_is_eligible() {
        assert!(is_document_request(Some("document"), None));
        assert!(is_document_request(Some("document"), Some("image/png")));
    }

    #[test]
    fn sec_fetch_dest_other_than_document_is_not_eligible() {
        for dest in [
            "script", "style", "image", "empty", "iframe", "font", "audio",
        ] {
            assert!(
                !is_document_request(Some(dest), Some("text/html")),
                "Sec-Fetch-Dest: {dest} must not be eligible"
            );
        }
    }

    #[test]
    fn accept_fallback_applies_only_without_sec_fetch_dest() {
        assert!(is_document_request(
            None,
            Some("text/html,application/xhtml+xml")
        ));
        assert!(!is_document_request(None, Some("application/json")));
        assert!(!is_document_request(None, None));
    }

    #[test]
    fn sec_fetch_dest_wins_over_accept() {
        assert!(!is_document_request(Some("empty"), Some("text/html")));
        assert!(!is_document_request(Some("script"), None));
    }

    #[test]
    fn sec_fetch_dest_is_case_insensitive_and_trimmed() {
        assert!(is_document_request(Some(" Document "), None));
    }

    // ── is_external_referer ─────────────────────────────────────

    #[test]
    fn same_site_referral_is_internal() {
        let idx = web_index();
        assert!(!is_external_referer("www.example.com", "web", &idx.domain));
        assert!(!is_external_referer("example.com", "web", &idx.domain));
    }

    #[test]
    fn other_site_referral_is_external() {
        let idx = web_index();
        assert!(is_external_referer("shop.example.net", "web", &idx.domain));
    }

    #[test]
    fn unknown_host_referral_is_external() {
        let idx = web_index();
        assert!(is_external_referer("news.example.org", "web", &idx.domain));
    }

    #[test]
    fn wildcard_domain_of_current_site_is_internal() {
        let idx = Indexes::build(vec![site("wild")], vec![domain("*.example.com", "wild")]);
        assert!(!is_external_referer("bar.example.com", "wild", &idx.domain));
        assert!(is_external_referer("example.org", "wild", &idx.domain));
    }

    #[test]
    fn cross_site_referral_is_external() {
        let idx = Indexes::build(
            vec![site("web"), site("shop")],
            vec![
                domain("example.com", "web"),
                domain("shop.example.net", "shop"),
            ],
        );
        assert!(is_external_referer("shop.example.net", "web", &idx.domain));
    }

    #[test]
    fn lookalike_host_is_not_internal() {
        // `notexample.com` must not be classified as internal for
        // `example.com` — the wildcard walk, not a suffix match,
        // is what keeps the two apart.
        let idx = web_index();
        assert!(is_external_referer("notexample.com", "web", &idx.domain));
    }
}
