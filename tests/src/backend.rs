//! Backend parsing integration tests.
//!
//! Covers: tests/CHECKLIST.md → Backend Parsing (4 tests)
//!
//! Tests parse_backend() from pangolin_core::parse to verify:
//!   - scheme detection (http/https/file)
//!   - tunnel prefix extraction (tun_name:url)
//!   - error handling (invalid scheme, invalid tun_name)

use pangolin_core::parse::{parse_backend, BackendScheme, ParseError};

/// backend_http — `http://host:port` → direct path, correct addr:port
#[test]
fn backend_http() {
    let result = parse_backend("http://127.0.0.1:8080");
    assert!(result.is_ok(), "http backend should parse ok");
    let (tun_name, url) = result.unwrap();
    assert_eq!(tun_name, "", "http should be direct (no tun)");
    assert_eq!(url, "http://127.0.0.1:8080");
}

/// backend_https — `https://host:port` → direct TLS
#[test]
fn backend_https() {
    let result = parse_backend("https://backend.example.com:8443");
    assert!(result.is_ok(), "https backend should parse ok");
    let (tun_name, url) = result.unwrap();
    assert_eq!(tun_name, "", "https should be direct (no tun)");
    assert_eq!(url, "https://backend.example.com:8443");
}

/// backend_file — `file:///path` → static file handler, no upstream
#[test]
fn backend_file() {
    let result = parse_backend("file:///var/www/static");
    assert!(result.is_ok(), "file backend should parse ok");
    let (tun_name, url) = result.unwrap();
    assert_eq!(tun_name, "", "file should be direct (no tun)");
    assert_eq!(url, "file:///var/www/static");
}

/// backend_tunnel_prefix — `office:http://x` → extracts tun_name=office, url=http://x
#[test]
fn backend_tunnel_prefix() {
    let result = parse_backend("office:http://192.168.1.100:8080");
    assert!(result.is_ok(), "tunnel backend should parse ok");
    let (tun_name, url) = result.unwrap();
    assert_eq!(tun_name, "office", "tun_name should be 'office'");
    assert_eq!(url, "http://192.168.1.100:8080");

    // Also test with https scheme
    let result = parse_backend("home:https://10.0.0.5:443");
    assert!(result.is_ok());
    let (tun_name, url) = result.unwrap();
    assert_eq!(tun_name, "home");
    assert_eq!(url, "https://10.0.0.5:443");
}

/// backend_tunnel_file — `tun_name:file:///path` extracts tun_name correctly
#[test]
fn backend_tunnel_file() {
    let result = parse_backend("office:file:///home/user/docs");
    assert!(result.is_ok());
    let (tun_name, url) = result.unwrap();
    assert_eq!(tun_name, "office");
    assert_eq!(url, "file:///home/user/docs");
}

/// backend_invalid_scheme — unsupported scheme → Err
#[test]
fn backend_unsupported_scheme() {
    let result = parse_backend("mailto:foo@bar.com");
    assert!(result.is_err(), "mailto should be unsupported");
    assert!(matches!(
        result.unwrap_err(),
        ParseError::UnsupportedScheme(_)
    ));

    let result = parse_backend("ftp://x.com");
    assert!(result.is_err(), "ftp should be unsupported");
}

/// backend_invalid_tun_name_digit_only — digit-only tun name rejected
#[test]
fn backend_invalid_tun_name_digit_only() {
    // "123:http://x" → 123 is digit-only tun_name, should be rejected
    // because pure digits are invalid tun names
    let result = parse_backend("123:http://x.com");
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), ParseError::InvalidTunName(_)));
}

/// backend_empty — empty string → Err
#[test]
fn backend_empty() {
    let result = parse_backend("");
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), ParseError::Empty));
}
