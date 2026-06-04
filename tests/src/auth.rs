//! Token auth integration tests.
//!
//! Covers: tests/CHECKLIST.md → Token Auth (7 tests, up from 5)

use std::sync::Arc;
use tempfile::TempDir;
use rusqlite::Connection;
use chrono::Utc;

use pangolin_core::db;
use pangolin_core::index::Indexes;
use pangolin_core::types::{Domain, Site, Token};

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

/// token_expiry_exactly_now_boundary — token expiring exactly now → inactive
///
/// When expires_at == now, `e > now` is false (boundary), so token is inactive.
/// This is correct: expiry means "no longer valid"
#[test]
fn token_expiry_exactly_now_boundary() {
    let token = Token {
        token: "exactly-expired".into(),
        enabled: true,
        created_at: Utc::now(),
        expires_at: Some(Utc::now()),
    };
    let indexes = make_indexes(vec![token]);

    // Expired at boundary = inactive
    assert_eq!(indexes.token.get("exactly-expired"), Some(&false));
}

/// token_mixed_state — multiple tokens with different states all indexed correctly
#[test]
fn token_mixed_state() {
    let tokens = vec![
        Token {
            token: "active-valid".into(),
            enabled: true,
            created_at: Utc::now(),
            expires_at: None,
        },
        Token {
            token: "active-future-expiry".into(),
            enabled: true,
            created_at: Utc::now(),
            expires_at: Some(Utc::now() + chrono::Duration::days(30)),
        },
        Token {
            token: "disabled-still-valid".into(),
            enabled: false,
            created_at: Utc::now(),
            expires_at: None,
        },
        Token {
            token: "disabled-and-expired".into(),
            enabled: false,
            created_at: Utc::now(),
            expires_at: Some(Utc::now() - chrono::Duration::days(1)),
        },
        Token {
            token: "active-but-expired".into(),
            enabled: true,
            created_at: Utc::now(),
            expires_at: Some(Utc::now() - chrono::Duration::hours(2)),
        },
    ];

    let indexes = make_indexes(tokens);

    assert_eq!(indexes.token.get("active-valid"), Some(&true));
    assert_eq!(indexes.token.get("active-future-expiry"), Some(&true));
    assert_eq!(indexes.token.get("disabled-still-valid"), Some(&false));
    assert_eq!(indexes.token.get("disabled-and-expired"), Some(&false));
    assert_eq!(indexes.token.get("active-but-expired"), Some(&false)); // expired wins
    assert_eq!(indexes.token.get("unknown-token"), None);
}