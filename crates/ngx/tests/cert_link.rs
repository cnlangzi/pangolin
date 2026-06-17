//! E2E tests for `pangolin_core::CertLinkCache` (fix/cert_www).
//!
//! These tests exercise the cache against a real SQLite connection
//! (in-memory) with the same schema the production code uses. They
//! cover the load/lookup/relink/remove lifecycle end-to-end and
//! verify the contract documented in `docs/design/cert-link.md`:
//!
//! - `load_from_db` rebuilds from `domains` × `certs` correctly
//! - `lookup` does an exact match, then a single-level wildcard walk
//! - `relink_for_domain` picks exact over wildcard
//! - `relink_for_cert` re-derives links for every affected domain
//! - `remove_domain` clears the entry
//! - Certs with non-`Issued` status are not used
//!
//! These tests do **not** need a Pebble ACME server — they're
//! pure DB-and-cache tests, fast and hermetic.

use std::sync::Arc;

use pangolin_core::cert_link::CertLinkCache;
use pangolin_core::db;
use pangolin_core::types::{Cert, CertStatus};

/// Build an in-memory SQLite connection with the production `domains`
/// and `certs` schemas (or as close as we need for these tests).
/// The full migration history is overkill here; we just create the
/// two tables and the columns we touch.
fn setup_db() -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
    conn.execute_batch(
        "CREATE TABLE domains (
             domain TEXT PRIMARY KEY,
             site_name TEXT,
             enabled INTEGER NOT NULL DEFAULT 1,
             auto_issue INTEGER NOT NULL DEFAULT 0,
             dns_provider TEXT,
             challenge_kind TEXT,
             created_at TEXT NOT NULL
         );
         CREATE TABLE certs (
             domain TEXT PRIMARY KEY,
             cert_file TEXT NOT NULL,
             key_file TEXT NOT NULL,
             expires_at TEXT,
             created_at TEXT NOT NULL,
             sans TEXT NOT NULL DEFAULT '[]',
             source TEXT NOT NULL DEFAULT 'manual',
             acme_dns_provider TEXT,
             acme_account_id TEXT,
             issued_at INTEGER NOT NULL DEFAULT 0,
             status TEXT NOT NULL DEFAULT 'issued',
             started_at TEXT,
             last_error TEXT,
             next_retry_at TEXT,
             error_class TEXT,
             attempt_count INTEGER NOT NULL DEFAULT 0,
             order_url TEXT
         );",
    )
    .expect("create schema");
    conn
}

/// Insert a domain row. Real sites / DNS providers are not relevant
/// for the cert-link logic, so we use placeholder values.
fn insert_domain(conn: &rusqlite::Connection, domain: &str) {
    conn.execute(
        "INSERT INTO domains(domain, site_name, created_at) VALUES (?1, 'test', '2026-01-01T00:00:00Z')",
        rusqlite::params![domain],
    )
    .expect("insert domain");
}

/// Insert a cert row with the given primary domain, SANs (as &str
/// slice), and status. Uses placeholder cert_file/key_file paths.
fn insert_cert(conn: &rusqlite::Connection, domain: &str, sans: &[&str], status: CertStatus) {
    let cert = Cert {
        domain: domain.to_string(),
        cert_file: format!("/tmp/{}.pem", domain),
        key_file: format!("/tmp/{}.pem", domain),
        expires_at: None,
        created_at: chrono::Utc::now(),
        sans: sans.iter().map(|s| s.to_string()).collect(),
        source: "acme".into(),
        acme_dns_provider: None,
        acme_account_id: None,
        issued_at: 0,
        status,
        started_at: None,
        last_error: None,
        next_retry_at: None,
        error_class: None,
        attempt_count: 0,
        order_url: None,
    };
    db::upsert_cert(conn, &cert).expect("insert cert");
}

// ---- load_from_db ----

#[test]
fn load_from_db_rebuilds_cache_from_exact_and_wildcard() {
    let conn = setup_db();
    insert_domain(&conn, "example.com");
    insert_domain(&conn, "api.example.com");
    insert_cert(
        &conn,
        "example.com",
        &["example.com", "*.example.com"],
        CertStatus::Issued,
    );

    let cache = CertLinkCache::load_from_db(&conn).expect("load");
    assert_eq!(cache.len(), 2);
    assert_eq!(
        cache.lookup("example.com").as_deref(),
        Some("example.com"),
        "exact match via cert.domain"
    );
    assert_eq!(
        cache.lookup("api.example.com").as_deref(),
        Some("example.com"),
        "single-level wildcard fallback"
    );
}

#[test]
fn load_from_db_skips_uncovered_domains() {
    let conn = setup_db();
    insert_domain(&conn, "example.com");
    insert_domain(&conn, "unrelated.org");
    insert_cert(
        &conn,
        "example.com",
        &["example.com", "*.example.com"],
        CertStatus::Issued,
    );

    let cache = CertLinkCache::load_from_db(&conn).expect("load");
    assert_eq!(cache.len(), 1);
    assert!(cache.lookup("unrelated.org").is_none());
}

#[test]
fn load_from_db_ignores_pending_and_failed_certs() {
    let conn = setup_db();
    insert_domain(&conn, "example.com");
    // A Pending cert must NOT be picked up — it's not yet usable.
    insert_cert(
        &conn,
        "example.com",
        &["example.com"],
        CertStatus::Pending,
    );
    // Same for Failed.
    insert_cert(
        &conn,
        "example.com",
        &["example.com"],
        CertStatus::Failed,
    );
    let cache = CertLinkCache::load_from_db(&conn).expect("load");
    assert!(
        cache.is_empty(),
        "Pending/Failed certs should not be linked"
    );
}

#[test]
fn load_from_db_uses_final_status_after_upsert() {
    // After upsert on conflict, the final state of the row is what
    // the cache sees. A "Failed then later retried to Issued" flow
    // lands as Issued; the cache picks it up.
    let conn = setup_db();
    insert_domain(&conn, "example.com");
    insert_cert(
        &conn,
        "example.com",
        &["example.com"],
        CertStatus::Failed,
    );
    insert_cert(
        &conn,
        "example.com",
        &["example.com"],
        CertStatus::Issued,
    );
    let cache = CertLinkCache::load_from_db(&conn).expect("load");
    assert_eq!(cache.lookup("example.com").as_deref(), Some("example.com"));
}

#[test]
fn load_from_db_handles_malformed_sans_json() {
    // A cert row whose `sans` column is not valid JSON should not
    // crash the relink — the cert is treated as if it had no SANs
    // (so it can only win via exact `cert.domain` match), and a
    // warning is logged naming the cert's primary. The operator
    // must find the bad row via that log and fix it.
    //
    // We bypass `db::upsert_cert` (which would serialize cleanly)
    // and write the bad row by hand.
    let conn = setup_db();
    insert_domain(&conn, "api.example.com");
    conn.execute(
        "INSERT INTO certs(domain, cert_file, key_file, created_at, sans, status)
         VALUES ('example.com', '/tmp/x.pem', '/tmp/x.pem',
                 '2026-01-01T00:00:00Z', 'this is not json', 'issued')",
        rusqlite::params![],
    )
    .expect("insert bad cert");

    let cache = CertLinkCache::load_from_db(&conn).expect("load");
    assert!(
        cache.is_empty(),
        "cert with malformed sans should not link to a subdomain"
    );
}

// ---- lookup ----

#[test]
fn lookup_walks_wildcard_keys() {
    let conn = setup_db();
    insert_domain(&conn, "*.example.com");
    insert_cert(
        &conn,
        "example.com",
        &["example.com", "*.example.com"],
        CertStatus::Issued,
    );
    let cache = CertLinkCache::load_from_db(&conn).expect("load");

    // SNI `api.example.com` is not a key in the cache (only
    // `*.example.com` is), so the lookup walks the labels and
    // matches the wildcard key.
    assert_eq!(
        cache.lookup("api.example.com").as_deref(),
        Some("example.com")
    );
}

#[test]
fn lookup_rejects_multi_level_subdomain() {
    // `*.example.com` covers `api.example.com` but not
    // `api.v2.example.com` (single-level wildcard). The cache
    // lookup, however, walks the labels; if a v2 wildcard key
    // is present, it'd be found. With only the base wildcard,
    // the walk must reach `*.example.com` (which would resolve)
    // — but the wildcard is in the cache only if a domain row
    // claimed it. With no `*.example.com` row, the walk has
    // nothing to match.
    let conn = setup_db();
    insert_domain(&conn, "api.example.com");
    insert_cert(
        &conn,
        "example.com",
        &["example.com", "*.example.com"],
        CertStatus::Issued,
    );
    let cache = CertLinkCache::load_from_db(&conn).expect("load");

    // api.example.com is in the cache (linked to example.com cert).
    assert_eq!(
        cache.lookup("api.example.com").as_deref(),
        Some("example.com")
    );
    // api.v2.example.com is NOT linked — there's no domain row for
    // it, so the cache doesn't know about it. (The link would only
    // exist if the user added `api.v2.example.com` to the `domains`
    // table and an exact or wildcard cert covered it.)
    assert!(cache.lookup("api.v2.example.com").is_none());
}

// ---- relink_for_domain ----

#[test]
fn relink_for_domain_picks_exact_over_wildcard() {
    let conn = setup_db();
    insert_domain(&conn, "api.example.com");
    // Wildcard cert first (lower priority), specific cert second.
    insert_cert(
        &conn,
        "example.com",
        &["*.example.com"],
        CertStatus::Issued,
    );
    insert_cert(
        &conn,
        "api.example.com",
        &["api.example.com"],
        CertStatus::Issued,
    );

    let cache = CertLinkCache::new();
    cache
        .relink_for_domain(&conn, "api.example.com")
        .expect("relink");
    assert_eq!(
        cache.lookup("api.example.com").as_deref(),
        Some("api.example.com"),
        "exact match should win over wildcard"
    );
}

#[test]
fn relink_for_domain_drops_entry_when_no_cert_covers() {
    let conn = setup_db();
    insert_domain(&conn, "example.com");
    insert_cert(
        &conn,
        "example.com",
        &["example.com"],
        CertStatus::Issued,
    );
    let cache = CertLinkCache::load_from_db(&conn).expect("load");
    assert!(!cache.is_empty());

    // Delete the only cert.
    db::delete_cert(&conn, "example.com").expect("delete cert");
    cache
        .relink_for_domain(&conn, "example.com")
        .expect("relink");
    assert!(cache.is_empty(), "no cert → no link");
}

// ---- relink_for_cert ----

#[test]
fn relink_for_cert_refreshes_all_affected_domains() {
    let conn = setup_db();
    insert_domain(&conn, "api.example.com");
    insert_domain(&conn, "www.example.com");
    insert_domain(&conn, "other.com");
    // No certs yet → cache empty.
    let cache = CertLinkCache::load_from_db(&conn).expect("load");
    assert!(cache.is_empty());

    // Add a wildcard cert.
    insert_cert(
        &conn,
        "example.com",
        &["example.com", "*.example.com"],
        CertStatus::Issued,
    );
    cache.relink_for_cert(&conn, "example.com").expect("relink");

    // *.example.com domains are now linked; other.com stays absent.
    assert_eq!(cache.len(), 2);
    assert_eq!(
        cache.lookup("api.example.com").as_deref(),
        Some("example.com")
    );
    assert_eq!(
        cache.lookup("www.example.com").as_deref(),
        Some("example.com")
    );
    assert!(cache.lookup("other.com").is_none());
}

// ---- remove_domain ----

#[test]
fn remove_domain_clears_entry() {
    let conn = setup_db();
    insert_domain(&conn, "example.com");
    insert_cert(
        &conn,
        "example.com",
        &["example.com"],
        CertStatus::Issued,
    );
    let cache = CertLinkCache::load_from_db(&conn).expect("load");
    assert_eq!(cache.len(), 1);

    cache.remove_domain("example.com");
    assert!(cache.is_empty());
    assert!(cache.lookup("example.com").is_none());
}

// ---- cert primary is bare domain for wildcard certs ----

#[test]
fn wildcard_cert_link_value_is_bare_domain() {
    // The cert issued for *.example.com is stored in the DB with
    // `certs.domain = "example.com"` (the bare primary, per the
    // SAN construction in acme.rs). The cache link's value should
    // be that bare domain — not the wildcard string.
    let conn = setup_db();
    insert_domain(&conn, "api.example.com");
    insert_cert(
        &conn,
        "example.com",
        &["example.com", "*.example.com"],
        CertStatus::Issued,
    );
    let cache = CertLinkCache::load_from_db(&conn).expect("load");
    assert_eq!(
        cache.lookup("api.example.com").as_deref(),
        Some("example.com"),
        "link value must be the cert primary (bare domain)"
    );
}

// ---- concurrent reads via Arc ----

#[test]
fn cache_is_cheap_to_clone() {
    // `CertLinkCache` clones share the underlying DashMap via Arc.
    // SNI callback and admin handlers each hold their own clone.
    let conn = setup_db();
    insert_domain(&conn, "example.com");
    insert_cert(
        &conn,
        "example.com",
        &["example.com"],
        CertStatus::Issued,
    );
    let cache = CertLinkCache::load_from_db(&conn).expect("load");
    let cache2 = cache.clone();

    // Both clones see the same data.
    assert_eq!(
        cache.lookup("example.com").as_deref(),
        Some("example.com")
    );
    assert_eq!(
        cache2.lookup("example.com").as_deref(),
        Some("example.com")
    );

    // A write through one clone is visible to the other.
    cache.remove_domain("example.com");
    assert!(cache2.lookup("example.com").is_none());
}

#[test]
fn cache_works_with_arc_wrapper() {
    // Production stores the cache inside `App` which is itself
    // `Arc<App>`. The cache's `Clone` is the way we share it.
    // This test just exercises the same code path through `Arc`.
    let conn = setup_db();
    insert_domain(&conn, "example.com");
    insert_cert(
        &conn,
        "example.com",
        &["example.com"],
        CertStatus::Issued,
    );
    let cache: Arc<CertLinkCache> =
        Arc::new(CertLinkCache::load_from_db(&conn).expect("load"));
    let cache2 = Arc::clone(&cache);
    assert_eq!(
        cache2.lookup("example.com").as_deref(),
        Some("example.com")
    );
}
