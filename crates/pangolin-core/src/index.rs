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

use std::collections::HashMap;
use std::sync::Arc;

use crate::normalize::normalize_host;
use crate::types::{Domain, Site, Token};

/// Site + its domains, as assembled in `Indexes::build`.
/// Used for `rebuild_tun_index` which needs the relationship.
#[derive(Debug, Clone)]
struct SiteWithDomains {
    site: Arc<Site>,
    domains: Vec<Arc<Domain>>,
}

/// All in-memory indexes. Wrapped in `Arc<RwLock<Indexes>>` at the
/// `ngx` layer for concurrent reads + atomic reload (see README).
#[derive(Debug, Default, Clone)]
pub struct Indexes {
    /// domain (exact or `*.wildcard` literal) → Site
    pub domain: HashMap<String, Arc<Site>>,
    /// tun_name → domains that route through that tun
    pub tun: HashMap<String, Vec<Arc<Domain>>>,
    /// token string → enabled (and not expired)
    pub token: HashMap<String, bool>,
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
    pub fn build(
        sites: Vec<Site>,
        domains: Vec<Domain>,
        tokens: &[Token],
        now: chrono::DateTime<chrono::Utc>,
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

        // 5. tokenIndex: enabled and not expired.
        let token_idx = build_token_index(tokens, now);

        Self {
            domain: domain_idx,
            tun: tun_idx,
            token: token_idx,
        }
    }
}

fn build_token_index(
    tokens: &[Token],
    now: chrono::DateTime<chrono::Utc>,
) -> HashMap<String, bool> {
    let mut idx = HashMap::new();
    for t in tokens {
        let active = t.enabled && t.expires_at.map(|e| e > now).unwrap_or(true);
        idx.insert(t.token.clone(), active);
    }
    idx
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
pub fn lookup_site(index: &Indexes, host: &str) -> Option<Arc<Site>> {
    let domain = normalize_host(host);

    // 1. exact match
    if let Some(site) = index.domain.get(&domain) {
        return Some(site.clone());
    }

    // 2. wildcard fall back: replace first label with `*`, re-lookup.
    //    Iteration 1: strip first label, prepend `*`.
    //    Iteration 2: strip first label of *that*, prepend `*`.
    //    ... until no more dots.
    let mut rest: &str = &domain;
    while let Some(dot) = rest.find('.') {
        rest = &rest[dot + 1..];
        let candidate = format!("*.{}", rest);
        if let Some(site) = index.domain.get(&candidate) {
            return Some(site.clone());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
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
            created_at: Utc::now(),
        }
    }

    #[allow(dead_code)]
    fn make_token(token: &str, enabled: bool) -> Token {
        Token {
            token: token.into(),
            enabled,
            created_at: Utc::now(),
            expires_at: None,
        }
    }

    #[test]
    fn exact_match() {
        let sites = vec![make_site("app", "http://127.0.0.1:8080")];
        let domains = vec![make_domain("app.example.com", "app")];
        let idx = Indexes::build(sites, domains, &[], Utc::now());
        let s = lookup_site(&idx, "app.example.com").unwrap();
        assert_eq!(s.name, "app");
    }

    #[test]
    fn exact_match_normalizes_case_and_port() {
        let sites = vec![make_site("app", "http://127.0.0.1:8080")];
        let domains = vec![make_domain("app.example.com", "app")];
        let idx = Indexes::build(sites, domains, &[], Utc::now());

        assert!(lookup_site(&idx, "App.Example.COM:8443").is_some());
    }

    #[test]
    fn wildcard_match() {
        let sites = vec![make_site("wild", "office:http://192.168.1.1:8080")];
        let domains = vec![make_domain("*.example.com", "wild")];
        let idx = Indexes::build(sites, domains, &[], Utc::now());
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
        let idx = Indexes::build(sites, domains, &[], Utc::now());
        let s = lookup_site(&idx, "x.foo.example.com").unwrap();
        assert_eq!(s.name, "long");
    }

    #[test]
    fn wildcard_with_tunnel_backend() {
        // Devin's correction: wildcard + tunnel is legitimate.
        let sites = vec![make_site("wild", "office:http://192.168.1.1:8080")];
        let domains = vec![make_domain("*.example.com", "wild")];
        let idx = Indexes::build(sites, domains, &[], Utc::now());
        let s = lookup_site(&idx, "foo.example.com").unwrap();
        assert_eq!(s.name, "wild");
        // tunIndex has 'office' (not empty)
        assert!(idx.tun.contains_key("office"));
    }

    #[test]
    fn no_match_returns_none() {
        let sites = vec![make_site("app", "http://x:8080")];
        let domains = vec![make_domain("app.example.com", "app")];
        let idx = Indexes::build(sites, domains, &[], Utc::now());
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
        let idx = Indexes::build(sites, domains, &[], Utc::now());
        assert_eq!(idx.tun.get("office").unwrap().len(), 2);
        assert_eq!(idx.tun.get("home").unwrap().len(), 1);
        assert!(!idx.tun.contains_key("direct"));
    }

    #[test]
    fn tun_index_strict_match_not_prefix() {
        // Critical: tun='home' must NOT match site backend='homestay:...'
        let sites = vec![make_site("a", "homestay:http://x:8080")];
        let domains = vec![make_domain("a.example.com", "a")];
        let idx = Indexes::build(sites, domains, &[], Utc::now());
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
            created_at: Utc::now(),
        }];
        let idx = Indexes::build(sites, domains, &[], Utc::now());
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
        let idx = Indexes::build(sites, domains, &[], Utc::now());
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
        let idx = Indexes::build(sites, domains, &[], Utc::now());
        assert!(lookup_site(&idx, "app.example.com").is_some());
        assert!(lookup_site(&idx, "orphan.example.com").is_none());
    }

    #[test]
    fn token_index_active_and_expired() {
        let now = Utc::now();
        let past = now - chrono::Duration::hours(1);
        let future = now + chrono::Duration::hours(1);

        let tokens = vec![
            Token {
                token: "active".into(),
                enabled: true,
                created_at: now,
                expires_at: None,
            },
            Token {
                token: "future".into(),
                enabled: true,
                created_at: now,
                expires_at: Some(future),
            },
            Token {
                token: "past".into(),
                enabled: true,
                created_at: now,
                expires_at: Some(past),
            },
            Token {
                token: "disabled".into(),
                enabled: false,
                created_at: now,
                expires_at: None,
            },
        ];
        let idx = Indexes::build(vec![], vec![], &tokens, now);
        assert_eq!(idx.token.get("active"), Some(&true));
        assert_eq!(idx.token.get("future"), Some(&true));
        assert_eq!(idx.token.get("past"), Some(&false));
        assert_eq!(idx.token.get("disabled"), Some(&false));
    }

    #[test]
    fn invalid_backend_excluded_from_tun_index() {
        // A site with a bad backend (unsupported scheme) is dropped from
        // tunIndex, but still in domainIndex (so a request still resolves
        // to the site; request-time parse will fail with 502).
        let sites = vec![make_site("a", "ftp://x:21")];
        let domains = vec![make_domain("a.example.com", "a")];
        let idx = Indexes::build(sites, domains, &[], Utc::now());
        // domainIndex has the site (we don't validate backend at build)
        assert!(lookup_site(&idx, "a.example.com").is_some());
        // but tunIndex is empty
        assert!(idx.tun.is_empty());
    }
}
