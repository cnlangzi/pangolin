//! Domain routing integration tests.
//!
//! Covers: tests/CHECKLIST.md → Domain Routing (6 tests)
//!
//! Each test:
//!   1. Builds in-memory indexes with a known site/domain configuration
//!   2. Calls pangolin_core::index::lookup_site() directly
//!   3. Asserts the returned Site matches expectations
//!
//! This is the fastest approach: no server process needed, pure unit-style
//! integration test of the routing logic.

use chrono::Utc;
use pangolin_core::index::{lookup_site, Indexes};
use pangolin_core::types::{Domain, HostMode, Site, Token};

// ---------------------------------------------------------------------------
// Helper: build indexes with a single site + domain
// ---------------------------------------------------------------------------

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
    let tokens = vec![Token {
        token: "test-token".into(),
        enabled: true,
        created_at: Utc::now(),
        expires_at: None,
    }];
    Indexes::build(sites, domains, &tokens, Utc::now())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// routing_exact_domain — exact `foo.example.com` → correct site
#[test]
fn routing_exact_domain() {
    let site = make_site("exact-test", "http://127.0.0.1:8080");
    let domain = make_domain("foo.example.com", "exact-test");
    let indexes = make_indexes(vec![site], vec![domain]);

    let result = lookup_site(&indexes, "foo.example.com");
    assert!(result.is_some(), "exact domain should be found");
    assert_eq!(result.unwrap().name, "exact-test");
}

/// routing_wildcard_single — `*.example.com` → matches request `bar.example.com`
#[test]
fn routing_wildcard_single() {
    let site = make_site("wildcard-test", "http://127.0.0.1:8080");
    let domain = make_domain("*.example.com", "wildcard-test");
    let indexes = make_indexes(vec![site], vec![domain]);

    let result = lookup_site(&indexes, "bar.example.com");
    assert!(result.is_some(), "subdomain should match wildcard");
    assert_eq!(result.unwrap().name, "wildcard-test");
}

/// routing_wildcard_subdomain — `foo.example.com` → matches `*.example.com`
#[test]
fn routing_wildcard_subdomain() {
    let site = make_site("wildcard-test", "http://127.0.0.1:8080");
    let domain = make_domain("*.example.com", "wildcard-test");
    let indexes = make_indexes(vec![site], vec![domain]);

    let result = lookup_site(&indexes, "foo.example.com");
    assert!(result.is_some(), "exact subdomain should match wildcard");
    assert_eq!(result.unwrap().name, "wildcard-test");
}

/// routing_case_insensitive — `Foo.Example.COM` normalized → match
#[test]
fn routing_case_insensitive() {
    let site = make_site("case-test", "http://127.0.0.1:8080");
    let domain = make_domain("*.example.com", "case-test");
    let indexes = make_indexes(vec![site], vec![domain]);

    let result = lookup_site(&indexes, "Foo.Example.COM");
    assert!(result.is_some(), "case-insensitive lookup should work");
    assert_eq!(result.unwrap().name, "case-test");
}

/// routing_port_stripped — `foo.com:8443` ≡ `foo.com`
#[test]
fn routing_port_stripped() {
    let site = make_site("port-test", "http://127.0.0.1:8080");
    let domain = make_domain("foo.com", "port-test");
    let indexes = make_indexes(vec![site], vec![domain]);

    let result = lookup_site(&indexes, "foo.com:8443");
    assert!(result.is_some(), "port should be stripped before lookup");
    assert_eq!(result.unwrap().name, "port-test");
}

/// routing_not_found — unknown domain → None
#[test]
fn routing_not_found() {
    let site = make_site("existing", "http://127.0.0.1:8080");
    let domain = make_domain("known.com", "existing");
    let indexes = make_indexes(vec![site], vec![domain]);

    let result = lookup_site(&indexes, "unknown.com");
    assert!(result.is_none(), "unknown domain should return None");
}
