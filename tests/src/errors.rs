//! Error handling integration tests.
//!
//! Covers: tests/CHECKLIST.md → Error Handling (3 tests)
//!
//! Tests error paths: not found, invalid request, upstream error simulation.

use chrono::Utc;
use pangolin_core::index::{lookup_site, Indexes};
use pangolin_core::parse::{parse_backend, ParseError};
use pangolin_core::types::{Domain, HostMode, Site, Token};
use std::collections::HashMap;

fn make_indexes(sites: Vec<Site>, domains: Vec<Domain>) -> Indexes {
    let tokens = vec![];
    Indexes::build(sites, domains, &tokens, Utc::now())
}

/// error_not_found — unknown domain → lookup returns None, ngx proxy returns 404
#[test]
fn error_not_found() {
    let site = make_site("existing", "http://127.0.0.1:8080");
    let domain = make_domain("known.com", "existing");
    let indexes = make_indexes(vec![site], vec![domain]);

    // Unknown domain returns None (ngx should turn this into 404)
    let result = lookup_site(&indexes, "unknown.com");
    assert!(result.is_none());
}

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
        created_at: Utc::now(),
    }
}

/// error_invalid_backend — invalid backend string → parse error
#[test]
fn error_invalid_backend() {
    // Empty
    assert!(parse_backend("").is_err());

    // Invalid scheme
    assert!(parse_backend("httpx://x.com").is_err());
    assert!(matches!(
        parse_backend("httpx://x.com").unwrap_err(),
        ParseError::UnsupportedScheme(_)
    ));

    // Digit-only tun name
    assert!(matches!(
        parse_backend("123:http://x.com").unwrap_err(),
        ParseError::InvalidTunName(_)
    ));
}

/// error_domain_disabled — disabled domain → excluded from index lookup
#[test]
fn error_domain_disabled() {
    let site = make_site("test-site", "http://127.0.0.1:8080");
    let domain = make_domain("disabled.com", "test-site");

    // Disabled domain
    let disabled_domain = Domain {
        domain: "disabled.com".into(),
        site_name: "test-site".into(),
        enabled: false, // <-- disabled
        created_at: Utc::now(),
    };

    let indexes = make_indexes(vec![site], vec![disabled_domain]);

    // Should not be found — disabled domains are excluded from index
    let result = lookup_site(&indexes, "disabled.com");
    assert!(result.is_none(), "disabled domain should return None");
}
