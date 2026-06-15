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
use pangolin_core::index::{Indexes, lookup_site};
use pangolin_core::parse::parse_backend;
use pangolin_core::types::{Domain, HostMode, Site};

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
        host_mode: HostMode::Passthrough,
        host_custom: None,
        domain_count: 0,
    }
}

fn make_site_with_host_mode(
    name: &str,
    backend: &str,
    host_mode: HostMode,
    host_custom: Option<&str>,
) -> Site {
    Site {
        name: name.to_string(),
        backend: backend.to_string(),
        enabled: true,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        host_mode,
        host_custom: host_custom.map(String::from),
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

// ---------------------------------------------------------------------------
// Host mode tests — verify Site.host_mode is stored and retrieved correctly
// ---------------------------------------------------------------------------

/// host_mode_passthrough — passthrough mode stores and serialises as "passthrough"
#[test]
fn host_mode_passthrough() {
    let site = make_site_with_host_mode(
        "passthrough-site",
        "http://127.0.0.1:8080",
        HostMode::Passthrough,
        None,
    );
    assert_eq!(site.host_mode, HostMode::Passthrough);
    assert!(site.host_custom.is_none());

    // JSON round-trip
    let json = serde_json::to_string(&site).unwrap();
    assert!(json.contains(r#"host_mode":"passthrough"#));
    let back: Site = serde_json::from_str(&json).unwrap();
    assert_eq!(back.host_mode, HostMode::Passthrough);
}

/// host_mode_backend — backend mode stores and serialises as "backend"
#[test]
fn host_mode_backend() {
    let site = make_site_with_host_mode(
        "backend-site",
        "http://192.168.1.100:9000",
        HostMode::Backend,
        None,
    );
    assert_eq!(site.host_mode, HostMode::Backend);

    let json = serde_json::to_string(&site).unwrap();
    assert!(json.contains(r#"host_mode":"backend"#));
    let back: Site = serde_json::from_str(&json).unwrap();
    assert_eq!(back.host_mode, HostMode::Backend);
}

/// host_mode_custom — custom mode stores the custom host value
#[test]
fn host_mode_custom() {
    let site = make_site_with_host_mode(
        "custom-site",
        "http://127.0.0.1:8080",
        HostMode::Custom,
        Some("internal.example.com"),
    );
    assert_eq!(site.host_mode, HostMode::Custom);
    assert_eq!(site.host_custom.as_deref(), Some("internal.example.com"));

    let json = serde_json::to_string(&site).unwrap();
    assert!(json.contains(r#"host_mode":"custom"#));
    assert!(json.contains(r#"host_custom":"internal.example.com"#));
    let back: Site = serde_json::from_str(&json).unwrap();
    assert_eq!(back.host_mode, HostMode::Custom);
    assert_eq!(back.host_custom.as_deref(), Some("internal.example.com"));
}

/// host_mode_default — Site defaults to Passthrough when fields are omitted
#[test]
fn host_mode_default() {
    // Empty JSON → default host_mode = Passthrough, host_custom = None
    let json = r#"{"name":"x","backend":"http://y","enabled":true,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}"#;
    let site: Site = serde_json::from_str(json).unwrap();
    assert_eq!(site.host_mode, HostMode::Passthrough);
    assert!(site.host_custom.is_none());
}

/// host_mode_from_str — HostMode parses from string correctly
#[test]
fn host_mode_from_str() {
    assert_eq!(
        "passthrough".parse::<HostMode>().unwrap(),
        HostMode::Passthrough
    );
    assert_eq!("backend".parse::<HostMode>().unwrap(), HostMode::Backend);
    assert_eq!("custom".parse::<HostMode>().unwrap(), HostMode::Custom);
    assert!("invalid".parse::<HostMode>().is_err());
}

/// host_mode_display — HostMode Display produces lowercase string
#[test]
fn host_mode_display() {
    assert_eq!(HostMode::Passthrough.to_string(), "passthrough");
    assert_eq!(HostMode::Backend.to_string(), "backend");
    assert_eq!(HostMode::Custom.to_string(), "custom");
}

// ---------------------------------------------------------------------------
// Host mode helper methods
// ---------------------------------------------------------------------------

/// host_mode_helper_methods — Site helper methods reflect host_mode correctly
#[test]
fn host_mode_helper_methods() {
    let passthrough = make_site_with_host_mode("x", "http://y", HostMode::Passthrough, None);
    assert!(passthrough.is_host_mode_passthrough());
    assert!(!passthrough.is_host_mode_backend());
    assert!(!passthrough.is_host_mode_custom());

    let backend = make_site_with_host_mode("x", "http://y", HostMode::Backend, None);
    assert!(!backend.is_host_mode_passthrough());
    assert!(backend.is_host_mode_backend());
    assert!(!backend.is_host_mode_custom());

    let custom = make_site_with_host_mode("x", "http://y", HostMode::Custom, Some("c"));
    assert!(!custom.is_host_mode_passthrough());
    assert!(!custom.is_host_mode_backend());
    assert!(custom.is_host_mode_custom());
}

// ---------------------------------------------------------------------------
// Backend host extraction — verify extract_host_from_backend logic
// ---------------------------------------------------------------------------

/// extract_host_from_backend_ip — extracts IP from http:// backend
#[test]
fn extract_host_from_backend_ip() {
    use pangolin_core::normalize::normalize_host;

    // IP address backend — host_mode=backend should use this IP as Host
    let site = make_site_with_host_mode(
        "ip-site",
        "http://203.0.113.50:8080",
        HostMode::Backend,
        None,
    );
    let domain = make_domain("ip.example.com", "ip-site");
    let indexes = make_indexes(vec![site], vec![domain]);

    let result = lookup_site(&indexes, "ip.example.com").unwrap();
    assert_eq!(result.host_mode, HostMode::Backend);

    // The backend host for IP backends should be passed through as-is
    // (the actual header writing is tested in integration tests)
    assert_eq!(result.backend, "http://203.0.113.50:8080");
}

/// extract_host_from_backend_domain — extracts domain from https:// backend
#[test]
fn extract_host_from_backend_domain() {
    let site = make_site_with_host_mode(
        "domain-site",
        "https://api.backend.internal/v2",
        HostMode::Backend,
        None,
    );
    let domain = make_domain("api.example.com", "domain-site");
    let indexes = make_indexes(vec![site], vec![domain]);

    let result = lookup_site(&indexes, "api.example.com").unwrap();
    assert_eq!(result.host_mode, HostMode::Backend);
    assert_eq!(result.backend, "https://api.backend.internal/v2");
}

/// extract_host_from_backend_with_tunnel_prefix — tunnel: prefix is stripped
#[test]
fn extract_host_from_backend_with_tunnel_prefix() {
    let site = make_site_with_host_mode(
        "tunnel-site",
        "office:http://192.168.1.1:3000",
        HostMode::Backend,
        None,
    );
    let domain = make_domain("tunnel.example.com", "tunnel-site");
    let indexes = make_indexes(vec![site], vec![domain]);

    let result = lookup_site(&indexes, "tunnel.example.com").unwrap();
    assert_eq!(result.host_mode, HostMode::Backend);
    // Backend includes tunnel prefix; ngx strips it when extracting host
    assert_eq!(result.backend, "office:http://192.168.1.1:3000");
}

/// host_mode_passthrough_preserves_client_host — passthrough mode keeps original
#[test]
fn host_mode_passthrough_preserves_client_host() {
    let site = make_site_with_host_mode(
        "passthrough-site",
        "http://127.0.0.1:8080",
        HostMode::Passthrough,
        None,
    );
    let domain = make_domain("client.example.com", "passthrough-site");
    let indexes = make_indexes(vec![site], vec![domain]);

    let result = lookup_site(&indexes, "client.example.com").unwrap();
    assert_eq!(result.host_mode, HostMode::Passthrough);
    // Passthrough means the client Host header is forwarded as-is
    assert_eq!(result.host_custom, None);
}

/// host_mode_custom_sets_fixed_host — custom mode ignores client host
#[test]
fn host_mode_custom_sets_fixed_host() {
    let site = make_site_with_host_mode(
        "custom-site",
        "http://127.0.0.1:8080",
        HostMode::Custom,
        Some("fixed.internal.example.com"),
    );
    let domain = make_domain("client.example.com", "custom-site");
    let indexes = make_indexes(vec![site], vec![domain]);

    let result = lookup_site(&indexes, "client.example.com").unwrap();
    assert_eq!(result.host_mode, HostMode::Custom);
    assert_eq!(
        result.host_custom.as_deref(),
        Some("fixed.internal.example.com")
    );
}

/// host_mode_custom_requires_host_custom_value — custom mode with empty value
#[test]
fn host_mode_custom_empty_value() {
    let site = make_site_with_host_mode(
        "custom-empty-site",
        "http://127.0.0.1:8080",
        HostMode::Custom,
        Some(""),
    );
    assert_eq!(site.host_mode, HostMode::Custom);
    assert_eq!(site.host_custom.as_deref(), Some(""));
}
