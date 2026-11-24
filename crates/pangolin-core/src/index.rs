//! In-memory indexes built from the SQL tables at startup (and rebuilt
//! on admin-triggered reload).
//!
//! **Devin's design**: a single `domainIndex` map keyed by the raw domain
//! string (including wildcard literals like `*.example.com`). Wildcard
//! lookups do not need a separate list — `lookup_site` rewrites the host
//! key on miss and re-queries the same map.
//!
//! This is intentionally simpler than the originally-proposed
//! `domainIndex + wildcardList` design (over-engineered, removed).
//!
//! v2: the in-memory `token` index is gone. WS auth is now a single
//! `auth_tun(name, token)` SQL query against the `tun` table, so there
//! is no need to mirror tokens into memory on startup.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use crate::normalize::normalize_host;
use crate::provider::{GlobalProviderConfig, SiteProviderConfig, SiteProviderOverride};
use crate::ratelimit::{IpRule, RateLimitPolicy, SiteLimitConfig, build_limit_configs};
use crate::types::{Domain, Site};

/// Site + its domains, as assembled in `Indexes::build`.
/// Used for `rebuild_tun_index` which needs the relationship.
#[derive(Debug, Clone)]
struct SiteWithDomains {
    site: Arc<Site>,
    domains: Vec<Arc<Domain>>,
}

/// All in-memory indexes. Wrapped in
/// `Arc<parking_lot::RwLock<Arc<Indexes>>>` in `App::indexes` at the
/// `pangolin-core` layer, for concurrent reads + atomic reload (see
/// `docs/design/routing-and-index.md`).
#[derive(Debug, Default, Clone)]
pub struct Indexes {
    /// domain (exact or `*.wildcard` literal) → Site
    pub domain: HashMap<String, Arc<Site>>,
    /// tun_name → domains that route through that tun
    pub tun: HashMap<String, Vec<Arc<Domain>>>,
    /// site name → effective rate-limit configuration (policy + IP
    /// rules).
    ///
    /// Built in the same pass that assembles the site index, so a
    /// reload swaps the routing table and the limits together — a
    /// request never sees the policy of one revision and the rules of
    /// another. One entry per site; a site without a policy row gets
    /// the default (limits off) policy.
    pub limits: HashMap<String, Arc<SiteLimitConfig>>,
    /// site name → effective provider configuration (which provider
    /// is in front of the site, the custom header names, the
    /// origin-guard switch and the site's manual allow ranges).
    ///
    /// Built in the same pass as `limits` and `domain`, so a reload
    /// swaps the routing table, the limits and the provider
    /// configuration together. One entry per site; a site without an
    /// override inherits the global defaults.
    pub providers: HashMap<String, Arc<SiteProviderConfig>>,
    /// site name → the first-level header names the site's provider
    /// consults.
    ///
    /// Precomputed when the revision is built: the request path used
    /// to build the list per request — a `Vec<String>` of names, and
    /// a `HashMap<String, String>` of the values it read off the
    /// request (issue #253). The names travel with the routing
    /// revision, so the request path reads them from here.
    ///
    /// One entry per site, exactly like [`Indexes::providers`]: the
    /// two maps are built from the same configurations, so a site
    /// always has both.
    pub provider_headers: HashMap<String, Arc<[String]>>,
    /// The domains that route through a tunnel — the subset of
    /// `domain` whose owning site has a `tun_name:` backend prefix.
    ///
    /// The set holds the **exact domain literals** of `domain`
    /// (wildcard literals included); the suffix match happens at
    /// lookup time in [`host_matches_set`].
    ///
    /// It lives inside the routing revision on purpose: the TLS ALPN
    /// callback reads it on the handshake path and the request path
    /// reads `domain`, and both have to agree on whether a host is
    /// tunnel-backed. Publishing them together removes the window in
    /// which the routing table already knew about a tunnel site while
    /// the handshake still classified it as direct (issue #212).
    pub tunnel_domains: Arc<HashSet<String>>,
    /// The tunnel node that serves each tunnel-backed domain — the
    /// same classification as [`Indexes::tunnel_domains`], resolved to
    /// the tun.
    ///
    /// The ALPN callback needs the tun to learn **which transport** its
    /// session runs on: a QUIC tunnel can negotiate h2, a
    /// WebSocket/yamux tunnel cannot. The map is built in the same
    /// pass as `tunnel_domains` out of the same `domain` map, so the
    /// two views can never disagree about which domains are
    /// tunnel-backed.
    pub tunnel_hosts: Arc<HashMap<String, String>>,
}

/// The tunnel classification of a routing revision: the domains that
/// route through a tunnel, and the tunnel node that serves each of
/// them.
///
/// The two views are handed out together because they have to be read
/// from the **same** revision: the ALPN callback classifies the SNI
/// against the domain set and then resolves the tun that serves it,
/// and a reload between the two reads would leave it with a domain set
/// from one revision next to a tun map from another (issue #212).
#[derive(Clone, Debug)]
pub struct TunnelClassification {
    /// Every tunnel-backed domain literal (exact FQDNs and
    /// `*.wildcard` wildcards).
    pub domains: Arc<HashSet<String>>,
    /// The tunnel node that serves each tunnel-backed domain.
    pub hosts: Arc<HashMap<String, String>>,
}

impl Indexes {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build indexes from the raw SQL data.
    ///
    /// `sites` and `domains` are joined in-memory (one site has many
    /// domains). The relationship is logical: `domains[i].site_name`
    /// must match a `sites[j].name`, otherwise the domain is dropped.
    pub fn build(sites: Vec<Site>, domains: Vec<Domain>) -> Self {
        Self::build_with_limits(sites, domains, &HashMap::new(), &[], &HashMap::new())
    }

    /// Build the indexes together with the per-site rate-limit
    /// configuration.
    ///
    /// `policies` holds the persisted policy of the sites that have
    /// one; `rules` holds the global and site-scoped IP rules;
    /// `bot_limits` holds the per-site list of known bots that are
    /// **not** exempt on that site (issue #131). A policy row whose
    /// site no longer exists is ignored, the same way a `domains` row
    /// without a matching site is ignored.
    pub fn build_with_limits(
        sites: Vec<Site>,
        domains: Vec<Domain>,
        policies: &HashMap<String, RateLimitPolicy>,
        rules: &[IpRule],
        bot_limits: &HashMap<String, BTreeSet<String>>,
    ) -> Self {
        Self::build_with_providers(
            sites,
            domains,
            policies,
            rules,
            bot_limits,
            &HashMap::new(),
            &HashMap::new(),
            &GlobalProviderConfig::default(),
        )
    }

    /// Build the indexes together with the per-site rate-limit
    /// configuration **and** the per-site provider configuration.
    ///
    /// `site_providers` holds the persisted per-site overrides (a site
    /// without one inherits the global defaults) and `site_ranges`
    /// holds the manual allow ranges of every site. `global` carries
    /// the global defaults — the provider, the custom header names
    /// and the origin-guard default.
    ///
    /// Everything is assembled in one pass so a reload swaps the
    /// routing table, the limits and the provider configuration
    /// together: a request never sees the site of one revision and
    /// the provider of another.
    // The index build takes every per-site input at once — the site
    // list, the rate-limit inputs, the provider inputs and the global
    // defaults. Grouping them into structs would only move the same
    // values one level down.
    #[allow(clippy::too_many_arguments)]
    pub fn build_with_providers(
        sites: Vec<Site>,
        domains: Vec<Domain>,
        policies: &HashMap<String, RateLimitPolicy>,
        rules: &[IpRule],
        bot_limits: &HashMap<String, BTreeSet<String>>,
        site_providers: &HashMap<String, SiteProviderOverride>,
        site_ranges: &HashMap<String, Vec<String>>,
        global: &GlobalProviderConfig,
    ) -> Self {
        // 1. Wrap sites in Arc and group domains by site_name.
        let mut sites_arc: Vec<Arc<Site>> = sites.into_iter().map(Arc::new).collect();
        // Sort sites by name for deterministic iteration order.
        sites_arc.sort_by(|a, b| a.name.cmp(&b.name));

        let mut by_site: HashMap<String, Vec<Arc<Domain>>> = HashMap::new();
        let domains_arc: Vec<Arc<Domain>> = domains.into_iter().map(Arc::new).collect();
        for d in &domains_arc {
            if !d.enabled {
                continue;
            }
            by_site
                .entry(d.site_name.clone())
                .or_default()
                .push(d.clone());
        }

        // 2. Build site-with-domains for the tun-index pass.
        let sites_with_domains: Vec<SiteWithDomains> = sites_arc
            .iter()
            .map(|s| SiteWithDomains {
                site: s.clone(),
                domains: by_site.get(&s.name).cloned().unwrap_or_default(),
            })
            .collect();

        // 3. domainIndex: enabled domains → site.
        let mut domain_idx: HashMap<String, Arc<Site>> = HashMap::new();
        for swd in &sites_with_domains {
            if !swd.site.enabled {
                continue;
            }
            for d in &swd.domains {
                domain_idx.insert(d.domain.clone(), swd.site.clone());
            }
        }

        // 4. tunIndex: sites whose backend has `tun_name:` prefix.
        let tun_idx = rebuild_tun_index(&sites_with_domains);

        // 5. limits: one entry per site, merged with the global rules.
        let site_names: Vec<String> = sites_arc.iter().map(|s| s.name.clone()).collect();
        let limits = build_limit_configs(&site_names, policies, rules, bot_limits);

        // 6. providers: one entry per site, the site's own override
        //    with the global defaults filled in.
        let providers = build_provider_configs(&site_names, site_providers, site_ranges, global);
        // 6b. The first-level header names of every site, precomputed
        //     out of the same configurations: the request path reads
        //     them from the revision instead of building the list per
        //     request (issue #253).
        let provider_headers: HashMap<String, Arc<[String]>> = providers
            .iter()
            .map(|(name, cfg)| (name.clone(), Arc::from(cfg.first_level_headers())))
            .collect();

        // 7. tunnel_domains: the domains of `domain_idx` whose owning
        //    site routes through a tunnel. Derived from the map that
        //    was just built, so the classification can never disagree
        //    with the routing table it belongs to.
        let (tunnel_domains, tunnel_hosts) = build_tunnel_classification(&domain_idx);

        Self {
            domain: domain_idx,
            tun: tun_idx,
            limits,
            providers,
            provider_headers,
            tunnel_domains: Arc::new(tunnel_domains),
            tunnel_hosts: Arc::new(tunnel_hosts),
        }
    }
}

/// Build the tunnel classification: the set of all domain strings
/// (exact + wildcard literals) whose owning site has a `tun_name:`
/// backend prefix, and the tunnel node that serves each of them.
///
/// The output contains the exact strings stored in `Indexes.domain`;
/// wildcard suffix matching is done at lookup time by
/// [`host_matches_set`] and [`host_lookup_tun`].
///
/// The decision belongs to the **site**, not to the domain: a site's
/// backend is parsed once and the answer reused for all of its
/// domains, so the pass costs one parse per site rather than one per
/// domain.
///
/// Sites whose `backend` cannot be parsed are skipped silently —
/// the same policy used in [`rebuild_tun_index`]. Any startup-time
/// parse errors have already failed-fast at boot, so a per-reload
/// silent skip is safe.
fn build_tunnel_classification(
    domain: &HashMap<String, Arc<Site>>,
) -> (HashSet<String>, HashMap<String, String>) {
    let mut domains = HashSet::new();
    let mut hosts = HashMap::new();
    // Site name → the tun that serves it (`None` for a direct site).
    // A domain's owner is decided once, no matter how many domains
    // the site has.
    let mut tunnel_sites: HashMap<&str, Option<String>> = HashMap::new();
    for (domain, site) in domain {
        let tun = tunnel_sites.entry(site.name.as_str()).or_insert_with(|| {
            match crate::parse::parse_backend(&site.backend) {
                Ok((tun, _)) if !tun.is_empty() => Some(tun),
                _ => None,
            }
        });
        if let Some(tun) = tun {
            domains.insert(domain.clone());
            hosts.insert(domain.clone(), tun.clone());
        }
    }
    (domains, hosts)
}

/// Build the effective provider configuration of every site.
///
/// A site without an override inherits every global default; a site
/// with one overrides the fields it sets and inherits the rest.
fn build_provider_configs(
    site_names: &[String],
    site_providers: &HashMap<String, SiteProviderOverride>,
    site_ranges: &HashMap<String, Vec<String>>,
    global: &GlobalProviderConfig,
) -> HashMap<String, Arc<SiteProviderConfig>> {
    site_names
        .iter()
        .map(|name| {
            let over = site_providers.get(name).cloned().unwrap_or_default();
            let mut cfg = global.resolve(&over);
            if let Some(cidrs) = site_ranges.get(name) {
                for cidr in cidrs {
                    // The stored ranges are canonical, so this cannot
                    // fail in practice; a malformed one (a hand-edited
                    // row) is skipped rather than taking the whole
                    // site down.
                    if let Err(e) = cfg.manual_ranges.insert(cidr) {
                        log::warn!("site {name}: ignoring invalid manual range: {e}");
                    }
                }
            }
            (name.clone(), Arc::new(cfg))
        })
        .collect()
}

/// Build the `tunIndex` map: tun_name → list of domains that route
/// through that tun.
///
/// Each site's `backend` field is parsed; if it has a `tun_name:`
/// prefix, the site's domains are appended to that tun's bucket.
/// Sites with a direct (no-prefix) backend contribute nothing.
///
/// **Strict equality** — `parse_backend` returns a single tun_name,
/// not a prefix. This is critical: `tun_name=home` must NOT match
/// site `homestay:...` (the kind of false-positive `HasPrefix` would
/// produce).
fn rebuild_tun_index(sites: &[SiteWithDomains]) -> HashMap<String, Vec<Arc<Domain>>> {
    let mut idx: HashMap<String, Vec<Arc<Domain>>> = HashMap::new();
    for swd in sites {
        let (tun_name, _) = match crate::parse::parse_backend(&swd.site.backend) {
            Ok(v) => v,
            Err(_) => continue, // invalid backend → skip (fail-fast at startup)
        };
        if tun_name.is_empty() {
            continue; // direct path, not in tunIndex
        }
        for d in &swd.domains {
            idx.entry(tun_name.clone()).or_default().push(d.clone());
        }
    }
    idx
}

/// Lookup a site by the request host.
///
/// 1. Normalize host (lowercase + strip port).
/// 2. Exact match in `domainIndex`.
/// 3. On miss, walk the host from the left, replacing each label
///    (the chunk between dots) with `*`, and try the map again.
///    First hit wins; earlier iterations match longer suffixes.
///
/// Examples:
///   `foo.example.com`            → exact miss → `*.example.com`
///   `foo.bar.example.com`        → exact miss → `*.bar.example.com`
///                                 (preferred over `*.example.com`)
///   `Foo.Example.COM:8443`       → normalize → `foo.example.com` → exact miss → `*.example.com`
///
/// The walk itself lives in [`site_of_host`], so the referer channel
/// can resolve a host against the same map without going through
/// the whole [`Indexes`].
pub fn lookup_site(index: &Indexes, host: &str) -> Option<Arc<Site>> {
    site_of_host(&index.domain, host).cloned()
}

/// Resolve the owning site of `host` against a domain → site map.
///
/// Same resolution rule as [`lookup_site`] — exact match first, then
/// the wildcard walk — but it takes the domain map directly instead of
/// the whole [`Indexes`], so a caller that already holds the map (or a
/// scoped copy of it) can resolve a host without taking the index lock
/// a second time.
///
/// The referer channel uses this for two things that must agree:
/// the request-time "is this referral external?" decision and the
/// query-time "which site does this row belong to?" grouping. Sharing
/// one function is what keeps the two from drifting — a hand-rolled
/// string comparison would classify `notexample.com` as internal for
/// `example.com`.
///
/// `domain` is expected to hold the same keys as `Indexes.domain`:
/// exact FQDNs (`foo.example.com`) and wildcard literals
/// (`*.example.com`).
pub fn site_of_host<'a>(
    domain: &'a HashMap<String, Arc<Site>>,
    host: &str,
) -> Option<&'a Arc<Site>> {
    let normalized = normalize_host(host);

    // 1. exact match
    if let Some(site) = domain.get(&normalized) {
        return Some(site);
    }

    // 2. wildcard fall back: replace first label with `*`, re-lookup.
    //    Iteration 1: strip first label, prepend `*`.
    //    Iteration 2: strip first label of *that*, prepend `*`.
    //    ... until no more dots.
    wildcard_candidates(&normalized).find_map(|candidate| domain.get(&candidate))
}

/// The wildcard candidates for a normalized host, longest suffix first.
///
/// `foo.bar.example.com` → `*.bar.example.com`, `*.example.com`, `*.com`.
///
/// Shared by [`site_of_host`] and [`host_matches_domains`] so the two
/// cannot drift: the map lookup and the plain-list lookup must agree
/// on what "this host belongs to that domain" means.
fn wildcard_candidates(normalized: &str) -> impl Iterator<Item = String> + '_ {
    let mut rest: &str = normalized;
    std::iter::from_fn(move || {
        let dot = rest.find('.')?;
        rest = &rest[dot + 1..];
        Some(format!("*.{rest}"))
    })
}

/// Does `host` match one of the given domain literals?
///
/// The same exact-plus-wildcard rule as [`site_of_host`], but over a
/// plain list of domain literals (exact FQDNs and `*.wildcard`
/// wildcards) instead of a `domain → site` map. Use it when the
/// caller holds the domain *strings* of one site — e.g. the admin
/// UI's site selector, which needs to scope a set of hosts to the
/// selected site's domains.
///
/// Sharing the walk with [`site_of_host`] is the point: a hand-rolled
/// suffix comparison would classify `notexample.com` as internal for
/// `example.com`, and the admin pages would then disagree with the
/// history/stats rollup (which resolves through `site_of_host`).
pub fn host_matches_domains(domains: &[String], host: &str) -> bool {
    let normalized = normalize_host(host);

    if domains.contains(&normalized) {
        return true;
    }

    wildcard_candidates(&normalized).any(|candidate| domains.contains(&candidate))
}

/// Check whether a host matches any entry in `set`, with the same
/// wildcard semantics as [`lookup_site`].
///
/// `set` is expected to contain domain strings in the same form
/// `Indexes.domain` does — exact FQDNs (`foo.example.com`) and
/// wildcard literals (`*.example.com`).
///
/// Used by the TLS ALPN callback to decide, per connection, whether
/// the SNI resolves to a tunnel-backed site. The lookup is sync
/// because the callback runs in a C context inside the handshake.
///
/// **Wildcard guard.** The walk must NOT consult `*.X` where `X`
/// has no further dot — i.e. a bare TLD like `*.com`. RFC 6125
/// (and Let's Encrypt's issuance rules) only allow wildcards over a
/// full suffix; `*.com` is meaningless and would, if present in
/// the set, cause every `*.com` host to be mis-classified as
/// tunnel. `cert_link::lookup` enforces the same rule (see the
/// `!rest.contains('.')` break in its walk) — keep them in sync.
pub fn host_matches_set(set: &std::collections::HashSet<String>, host: &str) -> bool {
    let domain = normalize_host(host);

    if set.contains(&domain) {
        return true;
    }

    guarded_wildcard_candidates(&domain).any(|candidate| set.contains(&candidate))
}

/// Resolve the tunnel node that serves `host`.
///
/// The twin of [`host_matches_set`] over a `domain → tun` map: the
/// same normalization, the same exact-then-wildcard walk and the same
/// bare-TLD guard, so the two answers can never disagree about which
/// hosts are tunnel-backed (issue #210).
///
/// `map` is the `Indexes.tunnel_hosts` view — the tunnel-backed
/// domain literals mapped to the tunnel node that serves them. A host
/// that is not in the map is not tunnel-backed; `None` is the answer
/// for a direct site *and* for a tunnel site whose entry the map does
/// not carry (a domain that moved between revisions).
///
/// The lookup is sync because the TLS ALPN callback runs in a C
/// context inside the handshake.
pub fn host_lookup_tun<'a>(map: &'a HashMap<String, String>, host: &str) -> Option<&'a str> {
    let domain = normalize_host(host);

    if let Some(tun) = map.get(&domain) {
        return Some(tun);
    }

    guarded_wildcard_candidates(&domain)
        .find_map(|candidate| map.get(&candidate))
        .map(|tun| tun.as_str())
}

/// The wildcard candidates for a normalized host, longest suffix first,
/// **stopping before a bare TLD**.
///
/// `foo.bar.example.com` → `*.bar.example.com`, `*.example.com` — and
/// then nothing: `*.com` is not a candidate, because a wildcard over a
/// bare TLD is meaningless (RFC 6125) and an entry `*.com` would
/// otherwise match every `foo.com` host.
///
/// This is the walk the TLS ALPN callback uses, and it is shared by
/// [`host_matches_set`] and [`host_lookup_tun`] so the classification
/// and the tun resolution cannot drift apart. The routing table's own
/// walk ([`wildcard_candidates`]) deliberately differs: it has no
/// TLD guard, and the difference is documented in
/// `docs/design/routing-and-index.md`.
fn guarded_wildcard_candidates(normalized: &str) -> impl Iterator<Item = String> + '_ {
    let mut rest: &str = normalized;
    std::iter::from_fn(move || {
        let dot = rest.find('.')?;
        rest = &rest[dot + 1..];
        // rest must contain a dot to be a valid wildcard suffix
        // (*.com is meaningless — wildcard certs don't cover TLDs).
        // Without this break, a "*.com" entry would match
        // "foo.com" and silently flip it to tunnel classification,
        // tripping the h2+yamux bug (issue #66).
        if !rest.contains('.') {
            return None;
        }
        Some(format!("*.{rest}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderKey;
    use crate::types::HostMode;
    use chrono::Utc;

    fn make_site(name: &str, backend: &str) -> Site {
        let now = Utc::now();
        Site {
            name: name.into(),
            backend: backend.into(),
            enabled: true,
            created_at: now,
            updated_at: now,
            host_mode: HostMode::Passthrough,
            host_custom: None,
            domain_count: 0,
        }
    }

    fn make_domain(domain: &str, site_name: &str) -> Domain {
        Domain {
            domain: domain.into(),
            site_name: site_name.into(),
            enabled: true,
            auto_issue: false,
            dns_provider: None,
            challenge_kind: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn exact_match() {
        let sites = vec![make_site("app", "http://127.0.0.1:8080")];
        let domains = vec![make_domain("app.example.com", "app")];
        let idx = Indexes::build(sites, domains);
        let s = lookup_site(&idx, "app.example.com").unwrap();
        assert_eq!(s.name, "app");
    }

    /// The first-level header names are precomputed into the routing
    /// revision (issue #253).
    ///
    /// The request path used to build the list per request — a
    /// `Vec<String>` of the names the provider consults, and a
    /// `HashMap<String, String>` of the values it read off the
    /// request. The revision carries the names now.
    #[test]
    fn the_provider_header_names_are_precomputed_into_the_revision() {
        let sites = vec![make_site("app", "http://127.0.0.1:8080")];
        let domains = vec![make_domain("app.example.com", "app")];
        let global = GlobalProviderConfig {
            provider: ProviderKey::Cloudflare,
            ..Default::default()
        };
        let idx = Indexes::build_with_providers(
            sites,
            domains,
            &HashMap::new(),
            &[],
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &global,
        );

        let names = idx
            .provider_headers
            .get("app")
            .expect("every site carries its header names");
        assert!(names.iter().any(|n| n == "CF-Connecting-IP"));
        assert!(names.iter().any(|n| n == "CF-IPCountry"));
        // The names come from the site's **effective** provider, so
        // they agree with the provider configuration of the same
        // revision.
        let provider = idx.providers.get("app").expect("every site has a provider");
        assert_eq!(names.as_ref(), provider.first_level_headers());
    }

    /// A `direct` site consults no first-level header at all — the
    /// list is empty, not a list of nothing-to-read.
    #[test]
    fn a_direct_site_has_no_first_level_header_names() {
        let sites = vec![make_site("app", "http://127.0.0.1:8080")];
        let domains = vec![make_domain("app.example.com", "app")];
        let idx = Indexes::build(sites, domains);
        let names = idx.provider_headers.get("app").expect("every site");
        assert!(names.is_empty());
    }

    /// `host_matches_domains` is the plain-list twin of
    /// `site_of_host` — the admin UI's site selector scopes a set of
    /// hosts through it, so the two must agree on every shape.
    #[test]
    fn host_matches_domains_agrees_with_site_of_host() {
        let sites = vec![make_site("web", "http://127.0.0.1:8080")];
        let domains = vec![
            make_domain("example.com", "web"),
            make_domain("*.example.org", "web"),
        ];
        let idx = Indexes::build(sites, domains);
        let literals: Vec<String> = idx.domain.keys().cloned().collect();

        for host in [
            "example.com",
            "EXAMPLE.COM",
            "example.com:8443",
            "foo.example.org",
            "a.b.example.org",
            "notexample.com",
            "example.org",
            "other.example.net",
        ] {
            assert_eq!(
                host_matches_domains(&literals, host),
                site_of_host(&idx.domain, host).is_some(),
                "the two host rules disagree on {host}"
            );
        }
    }

    /// The exact-plus-wildcard rule the router uses: a `*.suffix`
    /// literal covers any host with that suffix and at least one more
    /// label, and a lookalike host is not covered.
    #[test]
    fn host_matches_domains_uses_the_router_wildcard_rules() {
        let domains = vec!["example.com".to_string(), "*.example.org".to_string()];
        assert!(host_matches_domains(&domains, "example.com"));
        assert!(host_matches_domains(&domains, "EXAMPLE.COM:8443"));
        assert!(host_matches_domains(&domains, "foo.example.org"));
        // `*.example.org` does not cover the bare suffix.
        assert!(!host_matches_domains(&domains, "example.org"));
        // A lookalike host is not a match.
        assert!(!host_matches_domains(&domains, "notexample.com"));
        assert!(!host_matches_domains(&domains, "other.example.net"));
        // An empty list matches nothing.
        assert!(!host_matches_domains(&[], "example.com"));
    }

    #[test]
    fn exact_match_normalizes_case_and_port() {
        let sites = vec![make_site("app", "http://127.0.0.1:8080")];
        let domains = vec![make_domain("app.example.com", "app")];
        let idx = Indexes::build(sites, domains);

        assert!(lookup_site(&idx, "App.Example.COM:8443").is_some());
    }

    #[test]
    fn wildcard_match() {
        let sites = vec![make_site("wild", "office:http://192.168.1.1:8080")];
        let domains = vec![make_domain("*.example.com", "wild")];
        let idx = Indexes::build(sites, domains);
        let s = lookup_site(&idx, "foo.example.com").unwrap();
        assert_eq!(s.name, "wild");
    }

    #[test]
    fn wildcard_longest_suffix_wins() {
        // Two wildcards: *.example.com and *.foo.example.com
        // x.foo.example.com should hit *.foo.example.com
        let sites = vec![
            make_site("short", "http://short:8080"),
            make_site("long", "http://long:8080"),
        ];
        let domains = vec![
            make_domain("*.example.com", "short"),
            make_domain("*.foo.example.com", "long"),
        ];
        let idx = Indexes::build(sites, domains);
        let s = lookup_site(&idx, "x.foo.example.com").unwrap();
        assert_eq!(s.name, "long");
    }

    #[test]
    fn wildcard_with_tunnel_backend() {
        // Devin's correction: wildcard + tunnel is legitimate.
        let sites = vec![make_site("wild", "office:http://192.168.1.1:8080")];
        let domains = vec![make_domain("*.example.com", "wild")];
        let idx = Indexes::build(sites, domains);
        let s = lookup_site(&idx, "foo.example.com").unwrap();
        assert_eq!(s.name, "wild");
        // tunIndex has 'office' (not empty)
        assert!(idx.tun.contains_key("office"));
    }

    #[test]
    fn no_match_returns_none() {
        let sites = vec![make_site("app", "http://x:8080")];
        let domains = vec![make_domain("app.example.com", "app")];
        let idx = Indexes::build(sites, domains);
        assert!(lookup_site(&idx, "other.com").is_none());
        assert!(lookup_site(&idx, "unknown.example.com").is_none());
    }

    #[test]
    fn tun_index_groups_by_tun_name() {
        let sites = vec![
            make_site("a", "office:http://a:8080"),
            make_site("b", "home:http://b:8080"),
            make_site("c", "office:http://c:8080"),
            make_site("d", "http://direct:8080"), // direct, not in tunIndex
        ];
        let domains = vec![
            make_domain("a1.example.com", "a"),
            make_domain("b1.example.com", "b"),
            make_domain("c1.example.com", "c"),
            make_domain("d1.example.com", "d"),
        ];
        let idx = Indexes::build(sites, domains);
        assert_eq!(idx.tun.get("office").unwrap().len(), 2);
        assert_eq!(idx.tun.get("home").unwrap().len(), 1);
        assert!(!idx.tun.contains_key("direct"));
    }

    #[test]
    fn tun_index_strict_match_not_prefix() {
        // Critical: tun='home' must NOT match site backend='homestay:...'
        let sites = vec![make_site("a", "homestay:http://x:8080")];
        let domains = vec![make_domain("a.example.com", "a")];
        let idx = Indexes::build(sites, domains);
        assert!(!idx.tun.contains_key("home"));
        assert!(idx.tun.contains_key("homestay"));
    }

    #[test]
    fn disabled_domain_excluded() {
        let sites = vec![make_site("app", "http://x:8080")];
        let domains = vec![Domain {
            domain: "app.example.com".into(),
            site_name: "app".into(),
            enabled: false,
            auto_issue: false,
            dns_provider: None,
            challenge_kind: None,
            created_at: Utc::now(),
        }];
        let idx = Indexes::build(sites, domains);
        assert!(lookup_site(&idx, "app.example.com").is_none());
    }

    #[test]
    fn disabled_site_excluded() {
        let sites = vec![Site {
            name: "app".into(),
            backend: "http://x:8080".into(),
            enabled: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            host_mode: HostMode::Passthrough,
            host_custom: None,
            domain_count: 0,
        }];
        let domains = vec![make_domain("app.example.com", "app")];
        let idx = Indexes::build(sites, domains);
        assert!(lookup_site(&idx, "app.example.com").is_none());
    }

    #[test]
    fn domain_with_no_matching_site_dropped() {
        // Domain references a site that doesn't exist; silently dropped.
        let sites = vec![make_site("app", "http://x:8080")];
        let domains = vec![
            make_domain("app.example.com", "app"),
            make_domain("orphan.example.com", "nonexistent"),
        ];
        let idx = Indexes::build(sites, domains);
        assert!(lookup_site(&idx, "app.example.com").is_some());
        assert!(lookup_site(&idx, "orphan.example.com").is_none());
    }

    #[test]
    fn invalid_backend_excluded_from_tun_index() {
        // A site with a bad backend (unsupported scheme) is dropped from
        // tunIndex, but still in domainIndex (so a request still resolves
        // to the site; request-time parse will fail with 502).
        let sites = vec![make_site("a", "ftp://x:21")];
        let domains = vec![make_domain("a.example.com", "a")];
        let idx = Indexes::build(sites, domains);
        // domainIndex has the site (we don't validate backend at build)
        assert!(lookup_site(&idx, "a.example.com").is_some());
        // but tunIndex is empty
        assert!(idx.tun.is_empty());
    }

    // ── host_matches_set tests (used by TLS ALPN callback) ──

    fn set(items: &[&str]) -> std::collections::HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn host_matches_set_exact() {
        let s = set(&["foo.example.com", "bar.example.com"]);
        assert!(host_matches_set(&s, "foo.example.com"));
        assert!(host_matches_set(&s, "bar.example.com"));
        assert!(!host_matches_set(&s, "baz.example.com"));
    }

    #[test]
    fn host_matches_set_wildcard_literal() {
        // `*.example.com` literal in the set matches any sub.
        let s = set(&["*.example.com"]);
        assert!(host_matches_set(&s, "foo.example.com"));
        assert!(host_matches_set(&s, "x.y.example.com"));
        // Multi-level wildcard: `*.foo.example.com` literal wins first.
        let s = set(&["*.example.com", "*.foo.example.com"]);
        assert!(host_matches_set(&s, "a.foo.example.com"));
        assert!(host_matches_set(&s, "b.example.com"));
    }

    /// Without the `!rest.contains('.')` guard in the wildcard
    /// walk, a `*.com` entry would match every `foo.com` host and
    /// silently classify it as tunnel. The fix-cert-valid review
    /// caught this asymmetry with `cert_link::lookup`, which has
    /// had the guard since the cache was introduced.
    #[test]
    fn host_matches_set_rejects_bare_tld_wildcard() {
        // The hypothetical bad case: set has "*.com".
        let s = set(&["*.com"]);
        // foo.com must NOT match — *.com is meaningless and would
        // mis-classify every .com host as tunnel.
        assert!(!host_matches_set(&s, "foo.com"));
        // example.com likewise.
        assert!(!host_matches_set(&s, "example.com"));
    }

    #[test]
    fn host_matches_set_normalizes_case_and_port() {
        let s = set(&["foo.example.com"]);
        assert!(host_matches_set(&s, "FOO.example.com:8443"));
        assert!(host_matches_set(&s, "Foo.Example.COM"));
    }

    #[test]
    fn host_matches_set_misses_apex_for_wildcard() {
        // `*.example.com` should NOT match the apex `example.com`
        // (the wildcard deformation only strips labels).
        let s = set(&["*.example.com"]);
        assert!(!host_matches_set(&s, "example.com"));
    }

    // ── host_lookup_tun tests (the tunnel the ALPN callback resolves) ──

    fn host_map(items: &[(&str, &str)]) -> HashMap<String, String> {
        items
            .iter()
            .map(|(domain, tun)| (domain.to_string(), tun.to_string()))
            .collect()
    }

    #[test]
    fn host_lookup_tun_resolves_the_tun_name() {
        let m = host_map(&[("foo.example.com", "office"), ("*.example.org", "home")]);
        assert_eq!(host_lookup_tun(&m, "foo.example.com"), Some("office"));
        assert_eq!(host_lookup_tun(&m, "a.example.org"), Some("home"));
        assert_eq!(host_lookup_tun(&m, "other.test"), None);
    }

    /// The wildcard literal is found by the same walk the
    /// classification uses, and the longest suffix wins.
    #[test]
    fn host_lookup_tun_wildcard_literal() {
        let m = host_map(&[("*.example.com", "office"), ("*.foo.example.com", "home")]);
        assert_eq!(host_lookup_tun(&m, "a.example.com"), Some("office"));
        assert_eq!(host_lookup_tun(&m, "a.foo.example.com"), Some("home"));
        // The apex is not covered by a wildcard.
        assert_eq!(host_lookup_tun(&m, "example.com"), None);
    }

    /// The bare-TLD guard: a `*.com` entry must never make a host
    /// tunnel-backed — the same rule `host_matches_set` applies.
    #[test]
    fn host_lookup_tun_rejects_bare_tld_wildcard() {
        let m = host_map(&[("*.com", "office")]);
        assert_eq!(host_lookup_tun(&m, "foo.com"), None);
        assert_eq!(host_lookup_tun(&m, "example.com"), None);
    }

    #[test]
    fn host_lookup_tun_normalizes_case_and_port() {
        let m = host_map(&[("foo.example.com", "office")]);
        assert_eq!(host_lookup_tun(&m, "FOO.example.com:8443"), Some("office"));
        assert_eq!(host_lookup_tun(&m, "Foo.Example.COM"), Some("office"));
    }

    /// The classification and the tun resolution answer the same
    /// question — "is this host tunnel-backed?" — and they must answer
    /// it the same way.
    ///
    /// They are two lookups over two containers (a set and a map), so
    /// a change to one that forgot the other would split the handshake
    /// path: the ALPN callback would classify a host as direct and
    /// resolve a tun for it, or the other way round.
    #[test]
    fn host_lookup_tun_agrees_with_host_matches_set() {
        let entries = [
            "foo.example.com",
            "*.example.com",
            "*.foo.example.com",
            "deep.sub.example.org",
            "*.com",
        ];
        let hosts = [
            "foo.example.com",
            "FOO.example.com:8443",
            "bar.example.com",
            "a.foo.example.com",
            "x.y.example.com",
            "example.com",
            "foo.com",
            "com",
            "deep.sub.example.org",
            "sub.example.org",
            "other.test",
            "",
        ];

        for entry in entries {
            let s = set(&[entry]);
            let m = host_map(&[(entry, "office")]);
            for host in hosts {
                assert_eq!(
                    host_matches_set(&s, host),
                    host_lookup_tun(&m, host).is_some(),
                    "the classification and the tun lookup disagree for host \
                     {host:?} against the entry {entry:?}"
                );
            }
        }
    }

    // ── tunnel_domains tests (the ALPN classification) ──

    /// The tunnel classification is derived from the same domain map
    /// the routing table is built from, so a mixed topology has to
    /// classify exactly the tunnel-backed domains — no more, no
    /// less. A direct backend, a `file://` backend, an unparseable
    /// backend, a disabled site and a disabled domain are all
    /// excluded; a wildcard literal of a tunnel site is included.
    #[test]
    fn tunnel_domains_collects_exactly_the_tunnel_backed_domains() {
        let mut disabled_site = make_site("disabled", "office:http://10.0.0.9:8080");
        disabled_site.enabled = false;
        let mut disabled_domain = make_domain("disabled-domain.example.com", "tun");
        disabled_domain.enabled = false;

        let sites = vec![
            make_site("direct", "http://127.0.0.1:8080"),
            make_site("tun", "office:http://10.0.0.1:8080"),
            make_site("file", "file:///srv/static"),
            make_site("bad", "ftp://x:21"),
            disabled_site,
        ];
        let domains = vec![
            make_domain("direct.example.com", "direct"),
            make_domain("tun.example.com", "tun"),
            make_domain("*.wild.example.com", "tun"),
            make_domain("file.example.com", "file"),
            make_domain("bad.example.com", "bad"),
            make_domain("disabled.example.com", "disabled"),
            disabled_domain,
        ];

        let idx = Indexes::build(sites, domains);

        let expected: HashSet<String> = ["tun.example.com", "*.wild.example.com"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(*idx.tunnel_domains, expected);
    }

    /// The migrated mirror tests: the classification is a property of
    /// the routing revision, so it is asserted through
    /// `indexes.tunnel_domains` instead of a separate set.
    #[test]
    fn build_tunnel_classification_collects_only_tunnel_sites() {
        let sites = vec![
            make_site("direct", "http://127.0.0.1:8080"),
            make_site("tun", "office:http://10.0.0.1:8080"),
            make_site("file", "file:///srv/static"),
        ];
        let domains = vec![
            make_domain("direct.example.com", "direct"),
            make_domain("tun.example.com", "tun"),
            make_domain("file.example.com", "file"),
        ];
        let idx = Indexes::build(sites, domains);
        assert_eq!(idx.tunnel_domains.len(), 1);
        assert!(idx.tunnel_domains.contains("tun.example.com"));
        assert!(!idx.tunnel_domains.contains("direct.example.com"));
        assert!(!idx.tunnel_domains.contains("file.example.com"));
    }

    #[test]
    fn build_tunnel_classification_includes_wildcard_literals() {
        let sites = vec![make_site("tun", "office:http://10.0.0.1:8080")];
        let domains = vec![make_domain("*.example.com", "tun")];
        let idx = Indexes::build(sites, domains);
        // The literal `*.example.com` is stored as-is so the TLS callback
        // can match SNI `foo.example.com` via the wildcard deformation.
        assert!(idx.tunnel_domains.contains("*.example.com"));
    }

    #[test]
    fn build_tunnel_classification_skips_disabled_sites() {
        // Indexes::build already excludes disabled sites from `domain`,
        // so the classification follows the same filter.
        let mut disabled = make_site("tun", "office:http://10.0.0.1:8080");
        disabled.enabled = false;
        let sites = vec![disabled];
        let domains = vec![make_domain("tun.example.com", "tun")];
        let idx = Indexes::build(sites, domains);
        assert!(idx.tunnel_domains.is_empty());
    }

    #[test]
    fn build_tunnel_classification_skips_invalid_backends() {
        // Bad backend → parse fails → domain excluded from the
        // classification. Same policy as the tun index pass in
        // `Indexes::build`.
        let sites = vec![make_site("bad", "ftp://x:21")];
        let domains = vec![make_domain("bad.example.com", "bad")];
        let idx = Indexes::build(sites, domains);
        assert!(idx.tunnel_domains.is_empty());
    }

    /// The map carries the tunnel node that serves each tunnel-backed
    /// domain — the ALPN callback resolves the SNI to a tun and then
    /// asks which transport that tun's session runs on.
    #[test]
    fn tunnel_hosts_maps_each_tunnel_domain_to_its_tun() {
        let sites = vec![
            make_site("direct", "http://127.0.0.1:8080"),
            make_site("tun", "office:http://10.0.0.1:8080"),
            make_site("other-tun", "home:http://10.0.0.2:8080"),
        ];
        let domains = vec![
            make_domain("direct.example.com", "direct"),
            make_domain("tun.example.com", "tun"),
            make_domain("*.wild.example.com", "tun"),
            make_domain("other.example.com", "other-tun"),
        ];
        let idx = Indexes::build(sites, domains);

        assert_eq!(
            idx.tunnel_hosts.get("tun.example.com").map(String::as_str),
            Some("office")
        );
        assert_eq!(
            idx.tunnel_hosts
                .get("*.wild.example.com")
                .map(String::as_str),
            Some("office")
        );
        assert_eq!(
            idx.tunnel_hosts
                .get("other.example.com")
                .map(String::as_str),
            Some("home")
        );
        assert!(
            !idx.tunnel_hosts.contains_key("direct.example.com"),
            "a direct site must not appear in the tun map"
        );

        // The map's keys are the tunnel-backed domains — the very set
        // the classification holds. The two views are built in one
        // pass, so they cannot drift apart.
        let map_keys: HashSet<String> = idx.tunnel_hosts.keys().cloned().collect();
        assert_eq!(map_keys, *idx.tunnel_domains);
    }
}
