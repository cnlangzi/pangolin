//! The provider registry — what sits in front of pangolin.
//!
//! A **provider** is a front-end CDN or load balancer that forwards
//! the visitor's request to pangolin. The registry is the single
//! place that knows about them: for each provider it holds the
//! first-level headers the visitor IP / geo are read from **and** the
//! source of the provider's published source-IP ranges.
//!
//! Keeping both halves in one registry is the point: a site's header
//! source and its range source can never disagree, and adding the
//! next front CDN is one-place work rather than an edit spread over
//! the header resolver, the settings page and a per-provider fetch
//! loop.
//!
//! ## The two layers
//!
//! | Layer | What it is |
//! | ----- | ---------- |
//! | [`ProviderSpec`] | the static, registry-owned description of a provider |
//! | [`SiteProviderConfig`] | the effective configuration of **one site**: which provider is in front of it, the custom header names (for `custom_lb`), the origin-guard switch and the site's manual allow ranges |
//!
//! A site without a `site_providers` row inherits the global defaults
//! (`system_config`); a site with one overrides them field by field.
//! [`GlobalProviderConfig::resolve`] performs that inheritance.
//!
//! ## The origin guard
//!
//! The guard accepts a request only when the **TCP peer** is inside the
//! site's effective allowlist — the provider's published ranges ∪ the
//! site's manual ranges. It compares the peer, never the resolved
//! visitor IP: behind a CDN / LB the visitor IP comes from a header
//! a bypassing client can forge, so comparing it would not stop a
//! bypass.

use std::net::IpAddr;

use serde::{Deserialize, Serialize};

use crate::cidr::CidrSet;
use crate::types::SystemConfig;

/// A front-end CDN or load balancer.
///
/// The wire values are `direct`, `cloudflare` and `custom_lb` — the
/// same strings the persisted rows and the settings form use, so the
/// rows written before the rename keep working.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKey {
    /// The TCP peer is the visitor. No header is consulted and the
    /// provider publishes no range list.
    #[default]
    Direct,
    /// Cloudflare is in front. The visitor IP is read from
    /// `CF-Connecting-IP` (and `CF-Connecting-IPv6`), and the
    /// published source-IP ranges come from cloudflare.com.
    Cloudflare,
    /// Operator-configured header names (a customer-owned load
    /// balancer). The header names are supplied by the operator; the
    /// provider publishes no range list.
    #[serde(rename = "custom_lb")]
    CustomLb,
}

impl ProviderKey {
    /// The registry key as it appears on the wire and in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderKey::Direct => "direct",
            ProviderKey::Cloudflare => "cloudflare",
            ProviderKey::CustomLb => "custom_lb",
        }
    }

    /// Every provider of the registry, in display order.
    ///
    /// The admin UI builds its provider dropdown from this list, so a
    /// provider added to the registry appears without touching a
    /// template.
    pub fn all() -> &'static [ProviderKey] {
        &[
            ProviderKey::Direct,
            ProviderKey::Cloudflare,
            ProviderKey::CustomLb,
        ]
    }
}

impl std::str::FromStr for ProviderKey {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "direct" => Ok(ProviderKey::Direct),
            "cloudflare" => Ok(ProviderKey::Cloudflare),
            "custom_lb" => Ok(ProviderKey::CustomLb),
            other => Err(format!("unknown provider: {other}")),
        }
    }
}

impl std::fmt::Display for ProviderKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The first-level headers a provider sets.
///
/// `None` means "this provider does not set that value". For
/// `custom_lb` every name is operator-configured and therefore comes
/// from the site / global configuration rather than from the
/// registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProviderHeaders {
    /// The header carrying the visitor's IPv4 address.
    pub client_ip: Option<&'static str>,
    /// The header carrying the visitor's IPv6 address.
    pub client_ipv6: Option<&'static str>,
    /// The header carrying the visitor's country code.
    pub country: Option<&'static str>,
    /// The header carrying the visitor's region code.
    pub region: Option<&'static str>,
    /// The fallback region header, consulted when `region` is absent.
    pub region_fallback: Option<&'static str>,
    /// The visitor-IP header may hold a synthetic Pseudo-IPv4
    /// (`240.0.0.0/4`); when the IPv6 header is present the IPv6 wins.
    pub pseudo_ipv4: bool,
}

/// One URL a provider publishes its source-IP ranges at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeUrl {
    /// The URL to fetch.
    pub url: &'static str,
    /// A short human label ("IPv4" / "IPv6") shown in the admin UI.
    pub label: &'static str,
}

/// How a provider's range list is encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeParser {
    /// Plain text, one address or CIDR per line (Cloudflare).
    TextLines,
}

/// Where a provider's published source-IP ranges come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeSource {
    /// The URLs to fetch. Each URL keeps its own `ETag` /
    /// `Last-Modified` validator, so one of them changing does not
    /// re-download the others.
    pub urls: &'static [RangeUrl],
    /// How the body is encoded.
    pub parser: RangeParser,
}

/// Everything the code base knows about one provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderSpec {
    /// The registry key.
    pub key: ProviderKey,
    /// The display label shown in the admin UI.
    pub label: &'static str,
    /// A one-paragraph explanation shown next to the label in the
    /// admin UI. It lives here so a provider added to the registry
    /// needs no template edit.
    pub description: &'static str,
    /// The first-level headers this provider sets.
    pub headers: ProviderHeaders,
    /// Where the provider's published source-IP ranges come from.
    /// `None` → the provider publishes no list; only manual ranges
    /// apply.
    pub ranges: Option<RangeSource>,
}

/// The `direct` provider — the TCP peer is the visitor.
const DIRECT_SPEC: ProviderSpec = ProviderSpec {
    key: ProviderKey::Direct,
    label: "Direct",
    description: "The TCP peer address is the visitor. No header lookup. Use when \
                  pangolin is reached directly (no CDN / LB).",
    headers: ProviderHeaders {
        client_ip: None,
        client_ipv6: None,
        country: None,
        region: None,
        region_fallback: None,
        pseudo_ipv4: false,
    },
    ranges: None,
};

/// The Cloudflare provider.
const CLOUDFLARE_SPEC: ProviderSpec = ProviderSpec {
    key: ProviderKey::Cloudflare,
    label: "Cloudflare",
    description: "Read the CF-Connecting-IP header. Cloudflare also publishes the \
                  source-IP ranges its edges connect from, which the origin guard uses.",
    headers: ProviderHeaders {
        client_ip: Some("CF-Connecting-IP"),
        client_ipv6: Some("CF-Connecting-IPv6"),
        country: Some("CF-IPCountry"),
        region: Some("CF-Region-Code"),
        region_fallback: Some("CF-Region"),
        pseudo_ipv4: true,
    },
    ranges: Some(RangeSource {
        urls: &[
            RangeUrl {
                url: "https://www.cloudflare.com/ips-v4",
                label: "IPv4",
            },
            RangeUrl {
                url: "https://www.cloudflare.com/ips-v6",
                label: "IPv6",
            },
        ],
        parser: RangeParser::TextLines,
    }),
};

/// The `custom_lb` provider — the header names are operator-supplied.
const CUSTOM_LB_SPEC: ProviderSpec = ProviderSpec {
    key: ProviderKey::CustomLb,
    label: "Custom LB",
    description: "Read the header names configured below — one per value. An unset \
                  name yields an empty value. The load balancer publishes no range \
                  list, so the origin guard uses the site's manual ranges only.",
    headers: ProviderHeaders {
        client_ip: None,
        client_ipv6: None,
        country: None,
        region: None,
        region_fallback: None,
        pseudo_ipv4: false,
    },
    ranges: None,
};

/// The registry lookup — the only way to reach a provider's spec.
pub fn spec(key: ProviderKey) -> &'static ProviderSpec {
    match key {
        ProviderKey::Direct => &DIRECT_SPEC,
        ProviderKey::Cloudflare => &CLOUDFLARE_SPEC,
        ProviderKey::CustomLb => &CUSTOM_LB_SPEC,
    }
}

/// The per-site provider override, as stored in `site_providers`.
///
/// Every field is optional: an unset field inherits the global
/// default. A site without a row at all inherits everything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SiteProviderOverride {
    /// Which provider is in front of the site. `None` → inherit the
    /// global default.
    pub provider: Option<ProviderKey>,
    /// The header carrying the visitor's IP (`custom_lb` only).
    /// `None` → inherit the global name.
    pub client_ip_header: Option<String>,
    /// The header carrying the visitor's country (`custom_lb` only).
    /// `None` → inherit the global name.
    pub country_header: Option<String>,
    /// The header carrying the visitor's region (`custom_lb` only).
    /// `None` → inherit the global name.
    pub region_header: Option<String>,
    /// The origin-guard switch. `None` → inherit the global default,
    /// `Some(false)` → off, `Some(true)` → on.
    pub origin_guard_enabled: Option<bool>,
}

/// The global provider defaults (`system_config`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GlobalProviderConfig {
    /// The provider in front of every site that does not override it.
    pub provider: ProviderKey,
    /// The global header name for the visitor's IP (`custom_lb` only).
    pub client_ip_header: Option<String>,
    /// The global header name for the visitor's country.
    pub country_header: Option<String>,
    /// The global header name for the visitor's region.
    pub region_header: Option<String>,
    /// The global default of the origin guard.
    pub origin_guard_enabled: bool,
}

impl GlobalProviderConfig {
    /// Read the global defaults out of the `system_config` singleton.
    pub fn from_system_config(cfg: &SystemConfig) -> Self {
        Self {
            provider: cfg.provider,
            client_ip_header: cfg.client_ip_header.clone(),
            country_header: cfg.client_country_header.clone(),
            region_header: cfg.client_region_header.clone(),
            origin_guard_enabled: cfg.origin_guard_enabled,
        }
    }

    /// The effective configuration of a site: its own override with
    /// the global defaults filled in.
    ///
    /// An unset site field falls back to the global one; the guard
    /// switch inherits as a tri-state (`None` → the global default).
    pub fn resolve(&self, over: &SiteProviderOverride) -> SiteProviderConfig {
        SiteProviderConfig {
            provider: over.provider.unwrap_or(self.provider),
            client_ip_header: over
                .client_ip_header
                .clone()
                .or_else(|| self.client_ip_header.clone()),
            country_header: over
                .country_header
                .clone()
                .or_else(|| self.country_header.clone()),
            region_header: over
                .region_header
                .clone()
                .or_else(|| self.region_header.clone()),
            origin_guard_enabled: over
                .origin_guard_enabled
                .unwrap_or(self.origin_guard_enabled),
            manual_ranges: CidrSet::new(),
        }
    }
}

/// The effective provider configuration of one site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteProviderConfig {
    /// Which provider is in front of the site.
    pub provider: ProviderKey,
    /// The header carrying the visitor's IP (`custom_lb` only).
    pub client_ip_header: Option<String>,
    /// The header carrying the visitor's country (`custom_lb` only).
    pub country_header: Option<String>,
    /// The header carrying the visitor's region (`custom_lb` only).
    pub region_header: Option<String>,
    /// Whether the origin guard is on for this site.
    pub origin_guard_enabled: bool,
    /// The site's manual allow ranges, unioned with the provider's
    /// published ones.
    pub manual_ranges: CidrSet,
}

impl Default for SiteProviderConfig {
    fn default() -> Self {
        Self {
            provider: ProviderKey::Direct,
            client_ip_header: None,
            country_header: None,
            region_header: None,
            origin_guard_enabled: false,
            manual_ranges: CidrSet::new(),
        }
    }
}

impl SiteProviderConfig {
    /// The registry entry of this site's provider.
    pub fn spec(&self) -> &'static ProviderSpec {
        spec(self.provider)
    }

    /// The site's effective allowlist: the provider's published ranges
    /// ∪ the site's manual ranges.
    ///
    /// `provider_ranges` is the in-memory range set of the site's
    /// provider (empty when the provider publishes none, or when the
    /// list has never been fetched).
    pub fn effective_allowlist(&self, provider_ranges: &CidrSet) -> CidrSet {
        let mut out = provider_ranges.clone();
        for cidr in self.manual_ranges.to_cidrs() {
            // The manual ranges are already normalised, so this
            // cannot fail — but a silent skip would hide a bug, so the
            // error is logged by the caller of `effective_allowlist`.
            let _ = out.insert(&cidr);
        }
        out
    }

    /// Whether `peer` is inside the site's effective allowlist.
    ///
    /// The comparison is against the **peer**, never the resolved
    /// visitor IP: behind a CDN / LB the visitor IP comes from a
    /// header a bypassing client can forge.
    ///
    /// An empty allowlist fails **open** (the request is not
    /// rejected): an empty list would otherwise reject every request,
    /// including the first one after a fresh install.
    pub fn allows_peer(&self, peer: Option<IpAddr>, provider_ranges: &CidrSet) -> bool {
        let Some(peer) = peer else {
            // No peer address at all — there is nothing to compare
            // against, so the request is not rejected.
            return true;
        };
        let allowlist = self.effective_allowlist(provider_ranges);
        if allowlist.is_empty() {
            return true;
        }
        allowlist.contains(peer)
    }
}

/// How many identical rejections are logged per provider: the first
/// one, then every 1000th.
///
/// Mirrors the rate limiter's own log rate-limit (`LOG_EVERY` in
/// `ratelimit`) — a bypass attempt must not be able to flood the log.
pub const GUARD_LOG_EVERY: u64 = 1000;

/// The origin-guard rejection counters — one per provider.
///
/// The counter doubles as the log rate-limit: the first rejection of a
/// provider is logged, then every [`GUARD_LOG_EVERY`]-th one.
///
/// Guard rejections are answered inside the request filter and
/// therefore never reach the access log, so without a counter the
/// operator would have no way of seeing bypass attempts at all.
#[derive(Debug, Default)]
pub struct GuardRejectionCounter {
    counts: parking_lot::Mutex<std::collections::HashMap<ProviderKey, u64>>,
}

impl GuardRejectionCounter {
    /// A counter with no rejections recorded.
    pub fn new() -> Self {
        Self::default()
    }

    /// Count one rejection of `provider`.
    ///
    /// Returns `true` when the rejection should be logged.
    pub fn record(&self, provider: ProviderKey) -> bool {
        let mut counts = self.counts.lock();
        let count = counts.entry(provider).or_insert(0);
        *count += 1;
        let n = *count;
        n == 1 || n.is_multiple_of(GUARD_LOG_EVERY)
    }

    /// The number of rejections of `provider` so far.
    pub fn get(&self, provider: ProviderKey) -> u64 {
        self.counts.lock().get(&provider).copied().unwrap_or(0)
    }

    /// The counters of every provider that saw a rejection.
    pub fn snapshot(&self) -> std::collections::HashMap<ProviderKey, u64> {
        self.counts.lock().clone()
    }
}

/// Parse a provider's range list.
///
/// `TextLines` is plain text with one address or CIDR per line
/// (Cloudflare). Blank lines are skipped.
///
/// A malformed line fails the **whole** parse: a truncated or
/// substituted body must not silently shrink the allowlist — the
/// caller keeps the stored ranges in force instead.
pub fn parse_ranges(parser: RangeParser, body: &str) -> Result<CidrSet, String> {
    match parser {
        RangeParser::TextLines => {
            let mut set = CidrSet::new();
            for (i, line) in body.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                set.insert(line)
                    .map_err(|e| format!("line {}: {e}", i + 1))?;
            }
            Ok(set)
        }
    }
}

/// The stored range list of one `(provider, url)` pair.
///
/// This is the persisted form of what the fetch service downloaded:
/// the ranges themselves plus the validators of the last successful
/// request and the outcome of the last attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderIpList {
    /// The provider the list belongs to.
    pub provider: ProviderKey,
    /// The URL the list was fetched from.
    pub url: String,
    /// The ranges, in their canonical text form, one per line.
    pub cidrs: String,
    /// When the list was last fetched (successfully or not).
    pub fetched_at: chrono::DateTime<chrono::Utc>,
    /// The `ETag` validator of the last successful response.
    pub etag: Option<String>,
    /// The `Last-Modified` validator of the last successful response.
    pub last_modified: Option<String>,
    /// `ok` after a successful fetch, `error` after a failed one.
    pub status: String,
    /// The error text of the last failed fetch.
    pub last_error: Option<String>,
}

/// The status of a range list that was fetched successfully.
pub const STATUS_OK: &str = "ok";
/// The status of a range list whose last fetch failed.
pub const STATUS_ERROR: &str = "error";

impl ProviderIpList {
    /// The stored ranges, parsed into a set.
    ///
    /// A malformed line is skipped rather than failing the whole set:
    /// the stored list was validated when it was written, so a
    /// malformed line can only come from a hand-edited row, and
    /// dropping it is safer than dropping every range with it.
    pub fn to_cidr_set(&self) -> CidrSet {
        let mut set = CidrSet::new();
        for line in self.cidrs.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let _ = set.insert(line);
        }
        set
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_ip::normalize_addr;
    use std::net::SocketAddr;

    fn cf_ranges() -> CidrSet {
        let mut set = CidrSet::new();
        set.insert("203.0.113.0/24").unwrap();
        set
    }

    #[test]
    fn the_cloudflare_spec_carries_its_headers_and_range_urls() {
        let s = spec(ProviderKey::Cloudflare);
        assert_eq!(s.key, ProviderKey::Cloudflare);
        assert_eq!(s.headers.client_ip, Some("CF-Connecting-IP"));
        assert_eq!(s.headers.client_ipv6, Some("CF-Connecting-IPv6"));
        assert_eq!(s.headers.country, Some("CF-IPCountry"));
        assert_eq!(s.headers.region, Some("CF-Region-Code"));
        assert_eq!(s.headers.region_fallback, Some("CF-Region"));
        assert!(s.headers.pseudo_ipv4);
        let ranges = s.ranges.expect("cloudflare publishes ranges");
        assert_eq!(ranges.parser, RangeParser::TextLines);
        let urls: Vec<&str> = ranges.urls.iter().map(|u| u.url).collect();
        assert_eq!(
            urls,
            vec![
                "https://www.cloudflare.com/ips-v4",
                "https://www.cloudflare.com/ips-v6"
            ]
        );
    }

    #[test]
    fn the_direct_spec_carries_no_headers_and_no_range_source() {
        let s = spec(ProviderKey::Direct);
        assert_eq!(s.headers.client_ip, None);
        assert_eq!(s.headers.client_ipv6, None);
        assert_eq!(s.headers.country, None);
        assert_eq!(s.headers.region, None);
        assert!(!s.headers.pseudo_ipv4);
        assert!(s.ranges.is_none());
    }

    #[test]
    fn the_custom_lb_spec_carries_no_fixed_headers_and_no_range_source() {
        let s = spec(ProviderKey::CustomLb);
        assert_eq!(s.headers.client_ip, None);
        assert_eq!(s.headers.country, None);
        assert_eq!(s.headers.region, None);
        assert!(!s.headers.pseudo_ipv4);
        assert!(s.ranges.is_none());
    }

    #[test]
    fn the_registry_holds_exactly_three_providers() {
        assert_eq!(ProviderKey::all().len(), 3);
        for key in ProviderKey::all() {
            assert_eq!(spec(*key).key, *key);
            assert!(!spec(*key).label.is_empty());
        }
    }

    #[test]
    fn the_wire_values_are_unchanged() {
        assert_eq!(ProviderKey::Direct.to_string(), "direct");
        assert_eq!(ProviderKey::Cloudflare.to_string(), "cloudflare");
        assert_eq!(ProviderKey::CustomLb.to_string(), "custom_lb");
        assert_eq!(
            "custom_lb".parse::<ProviderKey>().unwrap(),
            ProviderKey::CustomLb
        );
        assert!("bogus".parse::<ProviderKey>().is_err());
    }

    #[test]
    fn a_site_without_an_override_inherits_the_global_provider() {
        let global = GlobalProviderConfig {
            provider: ProviderKey::Cloudflare,
            client_ip_header: None,
            country_header: None,
            region_header: None,
            origin_guard_enabled: false,
        };
        let cfg = global.resolve(&SiteProviderOverride::default());
        assert_eq!(cfg.provider, ProviderKey::Cloudflare);
    }

    #[test]
    fn a_site_with_an_override_wins_over_the_global_provider() {
        let global = GlobalProviderConfig {
            provider: ProviderKey::Cloudflare,
            client_ip_header: None,
            country_header: None,
            region_header: None,
            origin_guard_enabled: false,
        };
        let over = SiteProviderOverride {
            provider: Some(ProviderKey::Direct),
            ..Default::default()
        };
        assert_eq!(global.resolve(&over).provider, ProviderKey::Direct);
    }

    #[test]
    fn an_unset_site_header_name_falls_back_to_the_global_one() {
        let global = GlobalProviderConfig {
            provider: ProviderKey::CustomLb,
            client_ip_header: Some("X-Global-IP".into()),
            country_header: Some("X-Global-Country".into()),
            region_header: None,
            origin_guard_enabled: false,
        };
        let over = SiteProviderOverride {
            provider: Some(ProviderKey::CustomLb),
            client_ip_header: Some("X-Site-IP".into()),
            ..Default::default()
        };
        let cfg = global.resolve(&over);
        assert_eq!(cfg.client_ip_header.as_deref(), Some("X-Site-IP"));
        assert_eq!(cfg.country_header.as_deref(), Some("X-Global-Country"));
        assert_eq!(cfg.region_header, None);
    }

    #[test]
    fn the_guard_switch_inherits_as_a_tri_state() {
        let global = GlobalProviderConfig {
            provider: ProviderKey::Direct,
            client_ip_header: None,
            country_header: None,
            region_header: None,
            origin_guard_enabled: true,
        };
        // Inherit → the global value.
        assert!(
            global
                .resolve(&SiteProviderOverride::default())
                .origin_guard_enabled
        );
        // Explicitly off → off, even though the global default is on.
        let over = SiteProviderOverride {
            origin_guard_enabled: Some(false),
            ..Default::default()
        };
        assert!(!global.resolve(&over).origin_guard_enabled);
        // Explicitly on → on, even though the global default is off.
        let global = GlobalProviderConfig {
            origin_guard_enabled: false,
            ..global
        };
        let over = SiteProviderOverride {
            origin_guard_enabled: Some(true),
            ..Default::default()
        };
        assert!(global.resolve(&over).origin_guard_enabled);
    }

    #[test]
    fn a_custom_lb_site_with_no_header_names_has_no_visitor_ip() {
        // R3: a site that selects `custom_lb` and sets no header
        // names, in a deployment whose global names are unset too,
        // has **no** authoritative visitor IP — the access log falls
        // back to the TCP peer.
        let global = GlobalProviderConfig {
            provider: ProviderKey::CustomLb,
            client_ip_header: None,
            country_header: None,
            region_header: None,
            origin_guard_enabled: false,
        };
        let over = SiteProviderOverride {
            provider: Some(ProviderKey::CustomLb),
            ..Default::default()
        };
        let cfg = global.resolve(&over);
        assert_eq!(cfg.client_ip_header, None);

        let peer: SocketAddr = "1.2.3.4:5555".parse().unwrap();
        let authoritative =
            crate::client_ip::resolve_authoritative(&cfg, Some(peer), |_| Some("9.9.9.9".into()));
        assert_eq!(authoritative, None, "there is no authoritative visitor IP");

        // The access log falls back to the peer.
        assert_eq!(
            crate::client_ip::log_value(authoritative, Some(peer)),
            "1.2.3.4"
        );
    }

    #[test]
    fn the_guard_accepts_a_peer_inside_the_allowlist() {
        let mut cfg = SiteProviderConfig {
            provider: ProviderKey::Cloudflare,
            origin_guard_enabled: true,
            ..Default::default()
        };
        cfg.manual_ranges.insert("198.51.100.0/24").unwrap();
        let ranges = cf_ranges();
        assert!(cfg.allows_peer(Some("203.0.113.7".parse().unwrap()), &ranges));
        assert!(cfg.allows_peer(Some("198.51.100.9".parse().unwrap()), &ranges));
        assert!(!cfg.allows_peer(Some("192.0.2.7".parse().unwrap()), &ranges));
    }

    #[test]
    fn the_guard_ignores_the_resolved_visitor_ip() {
        // The guard only ever sees the peer — a forged header cannot
        // widen the allowlist.
        let mut cfg = SiteProviderConfig {
            provider: ProviderKey::Cloudflare,
            origin_guard_enabled: true,
            ..Default::default()
        };
        cfg.manual_ranges.insert("198.51.100.0/24").unwrap();
        let ranges = cf_ranges();
        let peer: IpAddr = "192.0.2.7".parse().unwrap();
        assert!(!cfg.allows_peer(Some(peer), &ranges));
    }

    #[test]
    fn an_empty_allowlist_fails_open() {
        let cfg = SiteProviderConfig {
            provider: ProviderKey::CustomLb,
            origin_guard_enabled: true,
            ..Default::default()
        };
        // No provider ranges and no manual ranges: nothing to compare
        // against, so the request is not rejected.
        assert!(cfg.allows_peer(Some("192.0.2.7".parse().unwrap()), &CidrSet::new()));
    }

    #[test]
    fn a_site_without_manual_ranges_accepts_only_the_provider_ranges() {
        let cfg = SiteProviderConfig {
            provider: ProviderKey::Cloudflare,
            origin_guard_enabled: true,
            ..Default::default()
        };
        let ranges = cf_ranges();
        assert!(cfg.allows_peer(Some("203.0.113.7".parse().unwrap()), &ranges));
        assert!(!cfg.allows_peer(Some("198.51.100.9".parse().unwrap()), &ranges));
    }

    #[test]
    fn a_direct_site_in_a_cloudflare_deployment_resolves_the_peer() {
        // R2: the visitor IP is resolved with the **effective
        // provider of the site being served**, not with the global
        // mode. A `direct` site inside a Cloudflare deployment must
        // therefore report the peer — and must ignore the
        // `CF-Connecting-IP` header a client could forge.
        let global = GlobalProviderConfig {
            provider: ProviderKey::Cloudflare,
            ..Default::default()
        };
        let over = SiteProviderOverride {
            provider: Some(ProviderKey::Direct),
            ..Default::default()
        };
        let cfg = global.resolve(&over);
        let peer: SocketAddr = "1.2.3.4:5555".parse().unwrap();
        let ip = crate::client_ip::resolve_authoritative(&cfg, Some(peer), |name| {
            if name == "CF-Connecting-IP" {
                Some("203.0.113.7".to_string())
            } else {
                None
            }
        });
        assert_eq!(ip, Some("1.2.3.4".parse().unwrap()));
    }

    #[test]
    fn a_cloudflare_site_in_a_direct_deployment_reads_the_header() {
        // R2, the other way round: a `cloudflare` site inside a
        // `direct` deployment reads the Cloudflare header, because
        // the site's own provider wins over the global default.
        let global = GlobalProviderConfig {
            provider: ProviderKey::Direct,
            ..Default::default()
        };
        let over = SiteProviderOverride {
            provider: Some(ProviderKey::Cloudflare),
            ..Default::default()
        };
        let cfg = global.resolve(&over);
        let peer: SocketAddr = "1.2.3.4:5555".parse().unwrap();
        let ip = crate::client_ip::resolve_authoritative(&cfg, Some(peer), |name| {
            if name == "CF-Connecting-IP" {
                Some("203.0.113.7".to_string())
            } else {
                None
            }
        });
        assert_eq!(ip, Some("203.0.113.7".parse().unwrap()));
    }

    #[test]
    fn a_text_list_of_ranges_is_parsed() {
        let set = parse_ranges(
            RangeParser::TextLines,
            "173.245.48.0/20\n103.21.244.0/22\n\n2400:cb00::/32\n",
        )
        .expect("parse");
        assert_eq!(set.len(), 3);
        assert!(set.contains("173.245.48.5".parse().unwrap()));
        assert!(set.contains("2400:cb00::1".parse().unwrap()));
    }

    #[test]
    fn a_malformed_line_fails_the_whole_parse() {
        // A truncated body must not silently shrink the allowlist.
        let err = parse_ranges(RangeParser::TextLines, "173.245.48.0/20\nnot-an-ip\n")
            .expect_err("must fail");
        assert!(err.contains("not-an-ip"), "{err}");
        assert!(err.contains("line 2"), "{err}");
    }

    #[test]
    fn the_rejection_counter_logs_the_first_and_every_thousandth() {
        let counter = GuardRejectionCounter::new();
        // The first rejection is logged…
        assert!(counter.record(ProviderKey::Cloudflare));
        // …the next 998 are not (the count reaches 999)…
        for _ in 0..998 {
            assert!(!counter.record(ProviderKey::Cloudflare));
        }
        // …and the 1000th is.
        assert!(counter.record(ProviderKey::Cloudflare));
        assert_eq!(counter.get(ProviderKey::Cloudflare), 1000);
        // The counters are per provider.
        assert_eq!(counter.get(ProviderKey::Direct), 0);
        assert!(counter.record(ProviderKey::Direct));
    }

    #[test]
    fn two_thousand_rejections_produce_at_most_three_log_lines() {
        // Mirrors the rate limiter's own log-rate contract: the first
        // rejection, then every 1000th.
        let counter = GuardRejectionCounter::new();
        let logged = (0..2000)
            .filter(|_| counter.record(ProviderKey::Cloudflare))
            .count();
        assert!(logged <= 3, "{logged} log lines");
        assert_eq!(counter.get(ProviderKey::Cloudflare), 2000);
    }

    #[test]
    fn a_normalized_peer_is_used_for_the_comparison() {
        let mut cfg = SiteProviderConfig {
            provider: ProviderKey::CustomLb,
            origin_guard_enabled: true,
            ..Default::default()
        };
        cfg.manual_ranges.insert("203.0.113.0/24").unwrap();
        // An IPv4-mapped IPv6 peer is the same address as its IPv4
        // form.
        let peer = normalize_addr("::ffff:203.0.113.7".parse().unwrap());
        assert!(cfg.allows_peer(Some(peer), &CidrSet::new()));
    }
}
