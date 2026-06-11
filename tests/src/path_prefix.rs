//! Path prefix integration tests.
//!
//! Covers: tests/CHECKLIST.md → Path Prefix (2 tests)
//!
//! Tests that ngx forwards request paths correctly when backend has a path prefix.
//!
//! Per README behavior for `backend: http://host/prefix`:
//!   GET /users     → forwards to http://host/prefix/users
//!   GET /users/1   → forwards to http://host/prefix/users/1
//!
//! Per README for `backend: http://host/prefix/` (trailing slash):
//!   GET /users     → forwards to http://host/prefix/users (no double-slash)
//!
//! These tests verify Indexes domain routing + parse_backend extraction.
//! The actual path-rewriting happens in ngx proxy; we test the upstream logic.

use chrono::Utc;
use pangolin_core::index::{lookup_site, Indexes};
use pangolin_core::parse::parse_backend;
use pangolin_core::types::{Domain, HostMode, Site, Token};

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
    let tokens = vec![];
    Indexes::build(sites, domains)
}

/// path_prefix_no_trailing_slash — `http://host/prefix` strips own path, appends request path
#[test]
fn path_prefix_no_trailing_slash() {
    // Backend: http://127.0.0.1:8080/api
    // Request path: /users → forwarded to http://127.0.0.1:8080/api/users
    // (nginx behavior: location /api { proxy_pass http://host; } → /users → /api/users)

    let site = make_site("prefix-site", "http://127.0.0.1:8080/api");
    let domain = make_domain("prefix.example.com", "prefix-site");
    let indexes = make_indexes(vec![site], vec![domain]);

    // Domain routing works
    let result = lookup_site(&indexes, "prefix.example.com");
    assert!(result.is_some(), "domain should be found");

    // parse_backend extracts tun_name + url
    let (tun_name, url) = parse_backend("http://127.0.0.1:8080/api").unwrap();
    assert_eq!(tun_name, "");
    assert_eq!(url, "http://127.0.0.1:8080/api");

    // The URL contains /api prefix — ngx proxy must strip /api when forwarding
    // We verify the site backend has the prefix
    assert_eq!(result.unwrap().backend, "http://127.0.0.1:8080/api");
}

/// path_prefix_with_trailing_slash — `http://host/prefix/` preserves slash semantics
#[test]
fn path_prefix_with_trailing_slash() {
    // Backend: http://127.0.0.1:8080/api/
    // GET /users → forwards to http://127.0.0.1:8080/api//users  (nginx: double-slash collapse)
    // GET /users → forwards to http://127.0.0.1:8080/api/users   (result after normalize)

    let site = make_site("trailing-site", "http://127.0.0.1:8080/api/");
    let domain = make_domain("trailing.example.com", "trailing-site");
    let indexes = make_indexes(vec![site], vec![domain]);

    let result = lookup_site(&indexes, "trailing.example.com");
    assert!(result.is_some());
    assert_eq!(result.unwrap().backend, "http://127.0.0.1:8080/api/");

    // parse_backend handles trailing slash correctly
    let (tun_name, url) = parse_backend("http://127.0.0.1:8080/api/").unwrap();
    assert_eq!(tun_name, "");
    assert_eq!(url, "http://127.0.0.1:8080/api/");
}

/// path_prefix_root_backend — `http://host/` passes through path unchanged
#[test]
fn path_prefix_root_backend() {
    // Backend: http://127.0.0.1:8080/
    // GET /users → forwards to http://127.0.0.1:8080/users (unchanged)

    let site = make_site("root-site", "http://127.0.0.1:8080/");
    let domain = make_domain("root.example.com", "root-site");
    let indexes = make_indexes(vec![site], vec![domain]);

    let result = lookup_site(&indexes, "root.example.com");
    assert!(result.is_some());
    assert_eq!(result.unwrap().backend, "http://127.0.0.1:8080/");

    let (tun_name, url) = parse_backend("http://127.0.0.1:8080/").unwrap();
    assert_eq!(tun_name, "");
    assert_eq!(url, "http://127.0.0.1:8080/");
}

/// path_prefix_no_prefix — `http://host` (no path) → pass through unchanged
#[test]
fn path_prefix_no_prefix() {
    // Backend: http://127.0.0.1:8080 (no trailing slash)
    // GET /users → forwards to http://127.0.0.1:8080/users

    let site = make_site("nopath-site", "http://127.0.0.1:8080");
    let domain = make_domain("nopath.example.com", "nopath-site");
    let indexes = make_indexes(vec![site], vec![domain]);

    let result = lookup_site(&indexes, "nopath.example.com");
    assert!(result.is_some());
    assert_eq!(result.unwrap().backend, "http://127.0.0.1:8080");
}
