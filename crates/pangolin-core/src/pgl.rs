//! The unified `P-*` gateway headers pangolin adds to every
//! forwarded request.
//!
//! Four headers, one meaning each:
//!
//! | Header              | Value                                    |
//! | ------------------- | ---------------------------------------- |
//! | `P-Known-Bot`       | the verified bot's registry id (or empty) |
//! | `P-Client-Ip`       | the authoritative visitor IP (or empty)    |
//! | `P-Client-Country`  | country code (or empty)                   |
//! | `P-Client-Region`   | region code (or empty)                    |
//!
//! The names use the `P-` prefix rather than `X-` because RFC 6648
//! reserves the `X-` prefix for non-standard fields and discourages
//! new uses of it. `P-` is a valid HTTP token, so the names survive
//! every proxy and framework in the path.
//!
//! ## Why the values live here (and not in `ngx`)
//!
//! Only the gateway (`ngx`) has both the CDN's first-level headers
//! and the knownbots verifier, so the values are computed there —
//! but the *computation* is pure and lives in `pangolin-core` so it
//! can be unit-tested without pingora. This module holds no
//! pingora dependency: it computes values, it does not write
//! headers. Writing them is `ngx`'s job, because the header
//! container there is pingora's `RequestHeader`.
//!
//! See `docs/design/gateway-headers.md` for the full contract.

use std::net::{IpAddr, SocketAddr};

use knownbots::Validator;

use crate::client_ip::resolve_authoritative;
use crate::provider::{ProviderKey, SiteProviderConfig, spec};

/// The registry id of the verified bot, empty when the request did
/// not come from one.
pub const HEADER_KNOWN_BOT: &str = "P-Known-Bot";

/// The authoritative visitor IP (normalized), empty when unknown.
pub const HEADER_CLIENT_IP: &str = "P-Client-Ip";

/// The visitor's country code, empty when unknown.
pub const HEADER_CLIENT_COUNTRY: &str = "P-Client-Country";

/// The visitor's region code, empty when unknown.
pub const HEADER_CLIENT_REGION: &str = "P-Client-Region";

/// Every gateway header name, in the canonical order.
///
/// Used to strip client-supplied copies before forwarding: a client
/// must never be able to impersonate a gateway header, so the names
/// are removed **unconditionally** — even when
/// `pgl_headers_enabled = false`.
pub const PGL_HEADER_NAMES: [&str; 4] = [
    HEADER_KNOWN_BOT,
    HEADER_CLIENT_IP,
    HEADER_CLIENT_COUNTRY,
    HEADER_CLIENT_REGION,
];

/// The computed values of the four gateway headers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PglHeaders {
    /// The registry id of the knownbots-**verified** crawler
    /// (`googlebot`, `gptbot`, …); `None` when the request did not
    /// come from one.
    pub bot_name: Option<String>,
    /// The authoritative client IP; `None` when there is none.
    pub client_ip: Option<IpAddr>,
    /// Country code; `None` when unknown.
    pub country: Option<String>,
    /// Region code; `None` when unknown.
    pub region: Option<String>,
}

impl PglHeaders {
    /// The `P-Known-Bot` value: the verified bot's registry id, or an
    /// empty string when there is no verified bot.
    ///
    /// The value is the same string the JSONL bot log writes as
    /// `bot_name` — never the raw User-Agent.
    pub fn known_bot_value(&self) -> &str {
        self.bot_name.as_deref().unwrap_or("")
    }

    /// The `P-Client-Ip` value: the normalized IP, empty when unknown.
    pub fn client_ip_value(&self) -> String {
        self.client_ip.map(|ip| ip.to_string()).unwrap_or_default()
    }

    /// The `P-Client-Country` value: the country code, empty when
    /// unknown.
    pub fn country_value(&self) -> String {
        self.country.clone().unwrap_or_default()
    }

    /// The `P-Client-Region` value: the region code, empty when
    /// unknown.
    pub fn region_value(&self) -> String {
        self.region.clone().unwrap_or_default()
    }

    /// The four `(name, value)` pairs, in the canonical order.
    pub fn to_pairs(&self) -> Vec<(&'static str, String)> {
        vec![
            (HEADER_KNOWN_BOT, self.known_bot_value().to_string()),
            (HEADER_CLIENT_IP, self.client_ip_value()),
            (HEADER_CLIENT_COUNTRY, self.country_value()),
            (HEADER_CLIENT_REGION, self.region_value()),
        ]
    }
}

/// Compute the four gateway header values.
///
/// Pure: no I/O, no async. `verifier` is `None` when the gateway
/// headers are disabled — that is how `P-Known-Bot` becomes empty
/// without any verification work being done. When a verifier is
/// present, `P-Known-Bot` carries the registry id of the bot only for
/// [`knownbots::VerifyStatus::Verified`]; pending / failed / unknown
/// all yield an empty value (fail closed), matching the existing
/// bot-log side channel.
///
/// The geo values are only read outside `direct` mode: in `direct`
/// mode a `CF-IPCountry` header could only have been sent by the
/// client, so trusting it would be spoofing.
///
/// `provider_cfg` is the **effective provider of the site being
/// served** — the site's own setting when it has one, the global
/// default otherwise. The header names therefore always come from
/// the same provider whose published ranges the origin guard uses.
pub fn resolve_headers<F>(
    provider_cfg: &SiteProviderConfig,
    peer: Option<SocketAddr>,
    get_header: F,
    user_agent: Option<&str>,
    verifier: Option<&Validator>,
) -> PglHeaders
where
    F: Fn(&str) -> Option<String>,
{
    let client_ip = resolve_authoritative(provider_cfg, peer, &get_header);
    let bot_name = match (verifier, user_agent, client_ip) {
        (Some(v), Some(ua), Some(ip)) => {
            let result = v.verify(ua, ip);
            if result.is_verified() {
                Some(result.name)
            } else {
                None
            }
        }
        _ => None,
    };
    PglHeaders {
        bot_name,
        client_ip,
        country: resolve_country(provider_cfg, &get_header),
        region: resolve_region(provider_cfg, &get_header),
    }
}

/// The first-level header names pangolin consults for the site's
/// effective provider.
///
/// `ngx` uses this to decide which headers to snapshot off the
/// request before the values are computed (the header lookup needs
/// an owned snapshot — the session cannot be borrowed immutably
/// while its request header is being mutated).
///
/// The fixed names come from the registry (see
/// [`crate::provider::spec`]); only `custom_lb` uses the
/// operator-configured names.
pub fn first_level_headers(provider_cfg: &SiteProviderConfig) -> Vec<String> {
    match provider_cfg.provider {
        ProviderKey::Direct => Vec::new(),
        ProviderKey::Cloudflare => {
            let headers = spec(ProviderKey::Cloudflare).headers;
            [
                headers.client_ip,
                headers.client_ipv6,
                headers.country,
                headers.region,
                headers.region_fallback,
            ]
            .into_iter()
            .flatten()
            .map(|n| n.to_string())
            .collect()
        }
        ProviderKey::CustomLb => {
            let mut names = Vec::new();
            for key in [
                provider_cfg.client_ip_header.as_deref(),
                provider_cfg.country_header.as_deref(),
                provider_cfg.region_header.as_deref(),
            ]
            .into_iter()
            .flatten()
            {
                if !names.iter().any(|n| n == key) {
                    names.push(key.to_string());
                }
            }
            names
        }
    }
}

/// The country code, or `None` when there is none.
fn resolve_country<F>(provider_cfg: &SiteProviderConfig, get_header: &F) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    match provider_cfg.provider {
        ProviderKey::Direct => None,
        ProviderKey::Cloudflare => {
            let name = spec(ProviderKey::Cloudflare).headers.country?;
            header_value(get_header, name)
        }
        ProviderKey::CustomLb => provider_cfg
            .country_header
            .as_deref()
            .and_then(|name| header_value(get_header, name)),
    }
}

/// The region code, or `None` when there is none.
///
/// Cloudflare exposes the region under two headers depending on the
/// zone / plan; `CF-Region-Code` wins when both are present.
fn resolve_region<F>(provider_cfg: &SiteProviderConfig, get_header: &F) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    match provider_cfg.provider {
        ProviderKey::Direct => None,
        ProviderKey::Cloudflare => {
            let headers = spec(ProviderKey::Cloudflare).headers;
            let primary = headers.region.and_then(|n| header_value(get_header, n));
            primary.or_else(|| {
                headers
                    .region_fallback
                    .and_then(|n| header_value(get_header, n))
            })
        }
        ProviderKey::CustomLb => provider_cfg
            .region_header
            .as_deref()
            .and_then(|name| header_value(get_header, name)),
    }
}

/// Read a header and return its trimmed value, or `None` when the
/// header is absent or empty.
fn header_value<F>(get_header: &F, name: &str) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    let raw = get_header(name)?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(provider: ProviderKey) -> SiteProviderConfig {
        SiteProviderConfig {
            provider,
            ..Default::default()
        }
    }

    fn headers(pairs: Vec<(&'static str, &'static str)>) -> impl Fn(&str) -> Option<String> {
        move |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_string())
        }
    }

    fn peer(a: u8, b: u8, c: u8, d: u8) -> Option<SocketAddr> {
        Some(SocketAddr::new(
            IpAddr::V4(std::net::Ipv4Addr::new(a, b, c, d)),
            0,
        ))
    }

    #[test]
    fn direct_mode_fills_ip_and_leaves_geo_empty() {
        let h = resolve_headers(
            &cfg(ProviderKey::Direct),
            peer(203, 0, 113, 7),
            headers(vec![]),
            None,
            None,
        );
        assert_eq!(h.client_ip_value(), "203.0.113.7");
        assert_eq!(h.country_value(), "");
        assert_eq!(h.region_value(), "");
        assert_eq!(h.known_bot_value(), "");
    }

    #[test]
    fn direct_mode_ignores_client_supplied_geo_headers() {
        let h = resolve_headers(
            &cfg(ProviderKey::Direct),
            peer(203, 0, 113, 7),
            headers(vec![("CF-IPCountry", "DE"), ("CF-Region-Code", "BE")]),
            None,
            None,
        );
        assert_eq!(h.country_value(), "");
        assert_eq!(h.region_value(), "");
    }

    #[test]
    fn cloudflare_mode_reads_geo_headers() {
        let h = resolve_headers(
            &cfg(ProviderKey::Cloudflare),
            peer(1, 2, 3, 4),
            headers(vec![
                ("CF-Connecting-IP", "203.0.113.7"),
                ("CF-IPCountry", "DE"),
                ("CF-Region-Code", "BE"),
            ]),
            None,
            None,
        );
        assert_eq!(h.client_ip_value(), "203.0.113.7");
        assert_eq!(h.country_value(), "DE");
        assert_eq!(h.region_value(), "BE");
    }

    #[test]
    fn cloudflare_region_falls_back_to_cf_region() {
        let h = resolve_headers(
            &cfg(ProviderKey::Cloudflare),
            peer(1, 2, 3, 4),
            headers(vec![("CF-Region", "BE")]),
            None,
            None,
        );
        assert_eq!(h.region_value(), "BE");
    }

    #[test]
    fn cloudflare_region_code_wins_over_region() {
        let h = resolve_headers(
            &cfg(ProviderKey::Cloudflare),
            peer(1, 2, 3, 4),
            headers(vec![("CF-Region-Code", "BE"), ("CF-Region", "XX")]),
            None,
            None,
        );
        assert_eq!(h.region_value(), "BE");
    }

    #[test]
    fn cloudflare_mode_ignores_the_configured_keys() {
        let mut c = cfg(ProviderKey::Cloudflare);
        c.client_ip_header = Some("X-Other-IP".into());
        c.country_header = Some("X-Other-Country".into());
        let h = resolve_headers(
            &c,
            peer(1, 2, 3, 4),
            headers(vec![
                ("CF-Connecting-IP", "203.0.113.7"),
                ("X-Other-IP", "198.51.100.9"),
                ("X-Other-Country", "FR"),
            ]),
            None,
            None,
        );
        assert_eq!(h.client_ip_value(), "203.0.113.7");
        assert_eq!(h.country_value(), "");
    }

    #[test]
    fn custom_lb_keys_are_independent() {
        let mut c = cfg(ProviderKey::CustomLb);
        c.country_header = Some("X-Country".into());
        let h = resolve_headers(
            &c,
            peer(1, 2, 3, 4),
            headers(vec![("X-Country", "DE"), ("X-Client-IP", "198.51.100.9")]),
            None,
            None,
        );
        assert_eq!(h.country_value(), "DE");
        assert_eq!(h.client_ip_value(), "");
        assert_eq!(h.region_value(), "");
    }

    #[test]
    fn custom_lb_reads_the_configured_ip_header() {
        let mut c = cfg(ProviderKey::CustomLb);
        c.client_ip_header = Some("X-Client-IP".into());
        let h = resolve_headers(
            &c,
            peer(1, 2, 3, 4),
            headers(vec![("X-Client-IP", "198.51.100.9")]),
            None,
            None,
        );
        assert_eq!(h.client_ip_value(), "198.51.100.9");
    }

    #[test]
    fn geo_values_are_trimmed() {
        let h = resolve_headers(
            &cfg(ProviderKey::Cloudflare),
            peer(1, 2, 3, 4),
            headers(vec![("CF-IPCountry", "  DE  ")]),
            None,
            None,
        );
        assert_eq!(h.country_value(), "DE");
    }

    #[test]
    fn empty_geo_values_are_reported_as_empty() {
        let h = resolve_headers(
            &cfg(ProviderKey::Cloudflare),
            peer(1, 2, 3, 4),
            headers(vec![("CF-IPCountry", "   ")]),
            None,
            None,
        );
        assert_eq!(h.country_value(), "");
    }

    /// A knownbots-verified crawler must produce `P-Known-Bot:
    /// googlebot` — the registry id, not a boolean and not the raw
    /// User-Agent.
    ///
    /// The verifier is built without background workers
    /// (`new_sync_only`) so the test never touches the network: the
    /// CIDR is seeded directly.
    #[test]
    fn verified_crawler_is_reported_as_a_known_bot() {
        let root = std::env::temp_dir().join("pangolin-pgl-knownbot-test");
        let validator = knownbots::Validator::new_sync_only(&root).expect("validator");
        validator
            .seed_prefix("googlebot", "203.0.113.0/24")
            .expect("seed googlebot CIDR");
        let h = resolve_headers(
            &cfg(ProviderKey::Direct),
            peer(203, 0, 113, 7),
            headers(vec![]),
            Some("Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)"),
            Some(&validator),
        );
        assert_eq!(
            h.known_bot_value(),
            "googlebot",
            "a verified crawler must be reported under its registry id"
        );
    }

    /// A forged User-Agent with an unrelated IP must NOT be reported
    /// as a known bot (fail closed).
    #[test]
    fn forged_user_agent_is_not_a_known_bot() {
        let root = std::env::temp_dir().join("pangolin-pgl-knownbot-test");
        let validator = knownbots::Validator::new_sync_only(&root).expect("validator");
        validator
            .seed_prefix("googlebot", "203.0.113.0/24")
            .expect("seed googlebot CIDR");
        let h = resolve_headers(
            &cfg(ProviderKey::Direct),
            peer(198, 51, 100, 9),
            headers(vec![]),
            Some("Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)"),
            Some(&validator),
        );
        assert_eq!(
            h.known_bot_value(),
            "",
            "a forged User-Agent from an unrelated IP must not verify"
        );
    }

    #[test]
    fn no_verifier_means_not_a_known_bot() {
        let h = resolve_headers(
            &cfg(ProviderKey::Direct),
            peer(203, 0, 113, 7),
            headers(vec![]),
            Some("Mozilla/5.0 (compatible; Googlebot/2.1)"),
            None,
        );
        assert_eq!(h.known_bot_value(), "");
    }

    #[test]
    fn no_client_ip_means_not_a_known_bot() {
        // Cloudflare mode without CF-Connecting-IP: there is no IP
        // to verify against, so the header must be empty.
        let h = resolve_headers(
            &cfg(ProviderKey::Cloudflare),
            peer(1, 2, 3, 4),
            headers(vec![]),
            Some("Mozilla/5.0 (compatible; Googlebot/2.1)"),
            None,
        );
        assert_eq!(h.known_bot_value(), "");
        assert_eq!(h.client_ip_value(), "");
    }

    #[test]
    fn pairs_carry_the_canonical_names() {
        let h = PglHeaders {
            bot_name: Some("googlebot".into()),
            client_ip: Some("203.0.113.7".parse().unwrap()),
            country: Some("DE".into()),
            region: Some("BE".into()),
        };
        let pairs = h.to_pairs();
        let names: Vec<&str> = pairs.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            vec![
                "P-Known-Bot",
                "P-Client-Ip",
                "P-Client-Country",
                "P-Client-Region"
            ]
        );
        assert_eq!(pairs[0].1, "googlebot");
        assert_eq!(pairs[1].1, "203.0.113.7");
        assert_eq!(pairs[2].1, "DE");
        assert_eq!(pairs[3].1, "BE");
    }

    /// The `P-Known-Bot` header is always written, even when there is
    /// no verified bot — the value is empty, not absent. A consumer
    /// therefore asks "is the value non-empty?", and the gateway's
    /// write path never has to branch.
    #[test]
    fn the_known_bot_header_is_always_present() {
        let h = PglHeaders::default();
        let pairs = h.to_pairs();
        let known = pairs
            .iter()
            .find(|(n, _)| *n == HEADER_KNOWN_BOT)
            .expect("the P-Known-Bot pair is always there");
        assert_eq!(known.1, "", "no verified bot → an empty value");
        assert_eq!(h.known_bot_value(), "");
    }

    /// A pending / failed verification is **not** a verified bot, so
    /// the header stays empty — the value is the registry id of a
    /// bot that actually verified.
    #[test]
    fn a_failed_verification_carries_no_bot_name() {
        let root = std::env::temp_dir().join("pangolin-pgl-knownbot-test");
        let validator = knownbots::Validator::new_sync_only(&root).expect("validator");
        validator
            .seed_prefix("googlebot", "203.0.113.0/24")
            .expect("seed googlebot CIDR");
        // The UA claims Googlebot but the IP is outside the seeded
        // range → the verification fails closed.
        let h = resolve_headers(
            &cfg(ProviderKey::Direct),
            peer(198, 51, 100, 9),
            headers(vec![]),
            Some("Mozilla/5.0 (compatible; Googlebot/2.1)"),
            Some(&validator),
        );
        assert_eq!(h.known_bot_value(), "");
        assert_eq!(h.bot_name, None);
    }

    #[test]
    fn first_level_headers_lists_the_consulted_names() {
        assert!(first_level_headers(&cfg(ProviderKey::Direct)).is_empty());
        let cf = first_level_headers(&cfg(ProviderKey::Cloudflare));
        assert!(cf.iter().any(|n| n == "CF-Connecting-IP"));
        assert!(cf.iter().any(|n| n == "CF-IPCountry"));

        let mut lb = cfg(ProviderKey::CustomLb);
        lb.client_ip_header = Some("X-Client-IP".into());
        lb.region_header = Some("X-Region".into());
        let names = first_level_headers(&lb);
        assert!(names.iter().any(|n| n == "X-Client-IP"));
        assert!(names.iter().any(|n| n == "X-Region"));
        assert!(!names.iter().any(|n| n == "X-Country"));
    }

    #[test]
    fn client_ip_normalizer_is_shared() {
        assert_eq!(
            crate::client_ip::normalize_ip("::ffff:203.0.113.7").map(|ip| ip.to_string()),
            Some("203.0.113.7".to_string())
        );
        assert_eq!(
            crate::client_ip::normalize_ip("203.0.113.7:80").map(|ip| ip.to_string()),
            Some("203.0.113.7".to_string())
        );
    }
}
