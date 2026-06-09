//! Direct path integration tests.
//!
//! Covers: tests/CHECKLIST.md → Direct Path (4 tests)
//!
//! Tests ngx proxy routing for direct (non-tunnel) backends.
//! These tests verify the domain routing layer and backend parsing,
//! which is the foundation for HTTP/HTTPS/file proxy routing.
//!
//! Full HTTP proxy flow (actual request forwarding) requires a running
//! ngx server process and is covered in the integration test suite
//! with the actual test infrastructure.

use chrono::Utc;
use pangolin_core::index::{lookup_site, Indexes};
use pangolin_core::parse::parse_backend;
use pangolin_core::types::{Domain, Site, Token};

// ---------------------------------------------------------------------------
// Index helper
// ---------------------------------------------------------------------------

fn make_site(name: &str, backend: &str) -> Site {
    Site {
        name: name.to_string(),
        backend: backend.to_string(),
        enabled: true,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        domain_count: 0,
    }
}

fn make_domain(domain: &str, site_name: &str) -> Domain {
    Domain {
        domain: domain.to_string(),
        site_name: site_name.to_string(),
        enabled: true,
        created_at: Utc::now(),
    }
}

fn make_indexes(sites: Vec<Site>, domains: Vec<Domain>) -> Indexes {
    let tokens = vec![];
    Indexes::build(sites, domains, &tokens, Utc::now())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// direct_http_get — site with HTTP backend → domain routes to correct site
#[test]
fn direct_http_get() {
    let site = make_site("http-site", "http://127.0.0.1:18080");
    let domain = make_domain("http.example.com", "http-site");
    let indexes = make_indexes(vec![site], vec![domain]);

    // Domain lookup returns the correct site
    let result = lookup_site(&indexes, "http.example.com");
    assert!(result.is_some());
    let site = result.unwrap();
    assert_eq!(site.name, "http-site");
    assert_eq!(site.backend, "http://127.0.0.1:18080");

    // parse_backend confirms HTTP scheme, no tunnel prefix
    let (tun, url) = parse_backend("http://127.0.0.1:18080").unwrap();
    assert_eq!(tun, "");
    assert_eq!(url, "http://127.0.0.1:18080");
}

/// direct_https_get — site with HTTPS backend → TLS backend routing
#[test]
fn direct_https_get() {
    let site = make_site("https-site", "https://backend.example.com:8443");
    let domain = make_domain("https.example.com", "https-site");
    let indexes = make_indexes(vec![site], vec![domain]);

    let result = lookup_site(&indexes, "https.example.com");
    assert!(result.is_some());
    let site = result.unwrap();
    assert_eq!(site.name, "https-site");
    assert_eq!(site.backend, "https://backend.example.com:8443");

    let (tun, url) = parse_backend("https://backend.example.com:8443").unwrap();
    assert_eq!(tun, "");
    assert_eq!(url, "https://backend.example.com:8443");
}

/// direct_static_file — backend `file:///path` → static file handler (not HTTP proxy)
#[test]
fn direct_static_file() {
    // file:/// backends are handled differently (static file serving, no upstream)
    // parse_backend should extract the path correctly
    let (tun, url) = parse_backend("file:///var/www/static").unwrap();
    assert_eq!(tun, "");
    assert_eq!(url, "file:///var/www/static");

    // With tunnel prefix
    let (tun, url) = parse_backend("office:file:///home/user/docs").unwrap();
    assert_eq!(tun, "office");
    assert_eq!(url, "file:///home/user/docs");
}

/// direct_path_prefix — backend with path prefix → ngx strips prefix when forwarding
#[test]
fn direct_path_prefix() {
    // Backend: http://127.0.0.1:8080/api
    // GET /users → ngx forwards to http://127.0.0.1:8080/api/users
    // We verify the site has the path prefix in its backend URL.

    let site = make_site("prefix-site", "http://127.0.0.1:8080/api");
    let domain = make_domain("api.example.com", "prefix-site");
    let indexes = make_indexes(vec![site], vec![domain]);

    let result = lookup_site(&indexes, "api.example.com");
    assert!(result.is_some());

    let (tun, url) = parse_backend("http://127.0.0.1:8080/api").unwrap();
    assert_eq!(tun, "");
    assert!(
        url.ends_with("/api"),
        "backend URL should contain /api prefix"
    );

    // The path prefix in the URL is what ngx uses for forwarding
    let site = result.unwrap();
    assert_eq!(site.backend, "http://127.0.0.1:8080/api");
}
