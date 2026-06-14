//! Wildcard routing integration tests.
//!
//! Covers: tests/CHECKLIST.md → Wildcard Routing (3 tests)

use chrono::Utc;
use pangolin_core::index::{Indexes, lookup_site};
use pangolin_core::types::{Domain, HostMode, Site};

fn make_site(name: &str, backend: &str) -> Site {
    Site {
        name: name.to_string(),
        backend: backend.to_string(),
        enabled: true,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        host_mode: HostMode::Passthrough,
        host_custom: None,
        domain_count: 0,
    }
}

fn make_domain(domain: &str, site_name: &str) -> Domain {
    Domain {
        domain: domain.to_string(),
        site_name: site_name.to_string(),
        enabled: true,
        auto_issue: false,
        dns_provider: None,
        created_at: Utc::now(),
    }
}

fn make_indexes(sites: Vec<Site>, domains: Vec<Domain>) -> Indexes {
    Indexes::build(sites, domains)
}

/// wildcard_deepest_match — `foo.bar.example.com` → matches `*.bar.example.com` before `*.example.com`
#[test]
fn wildcard_deepest_match() {
    let site1 = make_site("wildcard-site", "http://127.0.0.1:8080");
    let site2 = make_site("wildcard-site-2", "http://127.0.0.1:9999");

    let d1 = make_domain("*.bar.example.com", "wildcard-site");
    let d2 = make_domain("*.example.com", "wildcard-site-2");

    let indexes = make_indexes(vec![site1, site2], vec![d1, d2]);

    // foo.bar.example.com should match the more specific *.bar.example.com
    let result = lookup_site(&indexes, "foo.bar.example.com");
    assert!(result.is_some());
    assert_eq!(result.as_ref().unwrap().name, "wildcard-site");

    // foo.example.com (only 2 labels) should match *.example.com
    let result = lookup_site(&indexes, "foo.example.com");
    assert!(result.is_some());
    assert_eq!(result.as_ref().unwrap().name, "wildcard-site-2");
}

/// wildcard_multi_domain_one_site — exact + wildcard share same backend
#[test]
fn wildcard_multi_domain_one_site() {
    let site = make_site("shared-site", "http://127.0.0.1:9000");
    let d1 = make_domain("app.example.com", "shared-site");
    let d2 = make_domain("*.example.com", "shared-site");

    let indexes = make_indexes(vec![site], vec![d1, d2]);

    // Exact match
    let result = lookup_site(&indexes, "app.example.com");
    assert!(result.is_some());
    let s1 = result.unwrap();
    assert_eq!(s1.backend, "http://127.0.0.1:9000");

    // Wildcard match — should find same site
    let result = lookup_site(&indexes, "foo.example.com");
    assert!(result.is_some());
    let s2 = result.unwrap();
    assert_eq!(s2.name, "shared-site");
    assert_eq!(s2.backend, "http://127.0.0.1:9000");
}

/// wildcard_invalid_rejected — multi-layer wildcard is rejected
#[test]
fn wildcard_invalid_rejected() {
    use pangolin_core::is_valid_domain;

    // Multi-layer wildcard should be invalid
    assert!(
        !is_valid_domain("*.*.example.com"),
        "multi-layer wildcard invalid"
    );
    assert!(
        !is_valid_domain("foo.*.example.com"),
        "mid-domain wildcard invalid"
    );

    // Valid forms
    assert!(is_valid_domain("*.example.com"));
    assert!(is_valid_domain("foo.example.com"));
}
