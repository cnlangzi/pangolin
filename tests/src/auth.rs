//! Token auth integration tests.
//!
//! Covers: tests/CHECKLIST.md → Token Auth (3 tests)

use pangolin_core::index::Indexes;
use pangolin_core::types::{Domain, Site, Token};
use chrono::Utc;

fn make_indexes(tokens: Vec<Token>) -> Indexes {
    let sites = vec![];
    let domains = vec![];
    Indexes::build(sites, domains, &tokens, Utc::now())
}

/// token_valid — valid, non-expired, enabled token → active in token index
#[test]
fn token_valid() {
    let token = Token {
        token: "valid-token-abc".into(),
        enabled: true,
        created_at: Utc::now(),
        expires_at: None, // never expires
    };
    let indexes = make_indexes(vec![token]);

    assert_eq!(indexes.token.get("valid-token-abc"), Some(&true));
}

/// token_disabled — disabled token → present but inactive
#[test]
fn token_disabled() {
    let token = Token {
        token: "disabled-token".into(),
        enabled: false,
        created_at: Utc::now(),
        expires_at: None,
    };
    let indexes = make_indexes(vec![token]);

    // Present but marked inactive (= false)
    assert_eq!(indexes.token.get("disabled-token"), Some(&false));
}

/// token_expired — past-expired token → present but inactive
#[test]
fn token_expired() {
    let token = Token {
        token: "expired-token".into(),
        enabled: true,
        created_at: Utc::now(),
        expires_at: Some(Utc::now() - chrono::Duration::hours(1)), // expired 1h ago
    };
    let indexes = make_indexes(vec![token]);

    // Expired = inactive (= false)
    assert_eq!(indexes.token.get("expired-token"), Some(&false));
}

/// token_not_found — unknown token → absent from token index
#[test]
fn token_not_found() {
    let indexes = make_indexes(vec![]);

    assert_eq!(indexes.token.get("unknown-token"), None);
}

/// token_future_expiry — future-expired token → still active
#[test]
fn token_future_expiry() {
    let token = Token {
        token: "future-token".into(),
        enabled: true,
        created_at: Utc::now(),
        expires_at: Some(Utc::now() + chrono::Duration::hours(1)), // expires in 1h
    };
    let indexes = make_indexes(vec![token]);

    // Not yet expired
    assert_eq!(indexes.token.get("future-token"), Some(&true));
}