//! Admin API DB-level integration tests.
//!
//! Covers: tests/CHECKLIST.md → Admin API (DB layer, 8 tests)
//!
//! Tests the DB layer directly (db::list_sites, upsert_site, etc.)
//! without requiring a running HTTP server.

use chrono::Utc;
use rusqlite::Connection;
use std::sync::Arc;
use tempfile::TempDir;

use pangolin_core::db;
use pangolin_core::types::{Cert, Domain, HostMode, Site, Tun};

fn temp_conn() -> (TempDir, Connection) {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db");
    let mut conn = Connection::open(&db_path).unwrap();
    db::migrate(&mut conn).unwrap();
    (dir, conn)
}

/// admin_sites_crud — create, list, update, delete a site
#[test]
fn admin_sites_crud() {
    let (_dir, conn) = temp_conn();

    let site = Site {
        name: "test-site".into(),
        backend: "http://127.0.0.1:8080".into(),
        enabled: true,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        host_mode: HostMode::Passthrough,
        host_custom: None,
        domain_count: 0,
    };

    // Create
    db::upsert_site(&conn, &site).unwrap();

    // List
    let sites = db::list_sites(&conn).unwrap();
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].name, "test-site");
    assert_eq!(sites[0].backend, "http://127.0.0.1:8080");

    // Update
    let mut updated = site.clone();
    updated.backend = "http://127.0.0.1:9000".into();
    updated.updated_at = Utc::now();
    db::upsert_site(&conn, &updated).unwrap();

    let sites = db::list_sites(&conn).unwrap();
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].backend, "http://127.0.0.1:9000");

    // Delete
    db::delete_site(&conn, "test-site").unwrap();
    let sites = db::list_sites(&conn).unwrap();
    assert!(sites.is_empty());
}

/// admin_domains_crud — create, list, delete a domain (FK to site)
#[test]
fn admin_domains_crud() {
    let (_dir, conn) = temp_conn();

    // Create site first
    let site = Site {
        name: "my-site".into(),
        backend: "http://127.0.0.1:8080".into(),
        enabled: true,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        host_mode: HostMode::Passthrough,
        host_custom: None,
        domain_count: 0,
    };
    db::upsert_site(&conn, &site).unwrap();

    // Create domain
    let domain = Domain {
        domain: "app.example.com".into(),
        site_name: "my-site".into(),
        enabled: true,
        auto_issue: false,
        dns_provider: None,
        created_at: Utc::now(),
    };
    db::upsert_domain(&conn, &domain).unwrap();

    // List
    let domains = db::list_domains(&conn).unwrap();
    assert_eq!(domains.len(), 1);
    assert_eq!(domains[0].domain, "app.example.com");
    assert_eq!(domains[0].site_name, "my-site");

    // Delete
    db::delete_domain(&conn, "app.example.com").unwrap();
    let domains = db::list_domains(&conn).unwrap();
    assert!(domains.is_empty());
}

/// admin_tun_crud — create, list, delete a tun node (now also
/// exercises the merged token column).
#[test]
fn admin_tun_crud() {
    let (_dir, conn) = temp_conn();

    let tun = Tun {
        name: "office".into(),
        token: Some("secret-token-123".into()),
        token_hash: None,
        enabled: true,
        online: false,
        registered_at: None,
        last_seen_at: None,
        expires_at: None,
    };

    // Create
    db::upsert_tun(&conn, &tun).unwrap();

    // List
    let tuns = db::list_tuns(&conn).unwrap();
    assert_eq!(tuns.len(), 1);
    assert_eq!(tuns[0].name, "office");
    assert_eq!(tuns[0].token.as_deref(), Some("secret-token-123"));

    // Update online
    let mut updated = tun.clone();
    updated.online = true;
    updated.last_seen_at = Some(Utc::now());
    db::upsert_tun(&conn, &updated).unwrap();

    let tuns = db::list_tuns(&conn).unwrap();
    assert!(tuns[0].online);

    // Delete
    db::delete_tun(&conn, "office").unwrap();
    let tuns = db::list_tuns(&conn).unwrap();
    assert!(tuns.is_empty());
}

/// admin_tun_auth — verify the (name, token) → enabled lookup that
/// the WS server uses for first-contact auth. Replaces the
/// pre-merge `admin_tokens_crud` test.
#[test]
fn admin_tun_auth() {
    let (_dir, conn) = temp_conn();
    db::upsert_tun(
        &conn,
        &Tun {
            name: "office".into(),
            token: Some("secret-token-123".into()),
            token_hash: None,
            enabled: true,
            online: false,
            registered_at: None,
            last_seen_at: None,
            expires_at: None,
        },
    )
    .unwrap();

    // Match → enabled row
    let r = db::auth_tun(&conn, "office", "secret-token-123").unwrap();
    assert_eq!(r.map(|(e, _)| e), Some(true));

    // Wrong token → no row
    assert!(db::auth_tun(&conn, "office", "wrong").unwrap().is_none());

    // Wrong name → no row
    assert!(db::auth_tun(&conn, "nope", "secret-token-123")
        .unwrap()
        .is_none());
}

/// admin_certs_crud — create, list, delete a cert
#[test]
fn admin_certs_crud() {
    let (_dir, conn) = temp_conn();

    let cert = Cert {
        domain: "example.com".into(),
        cert_file: "/certs/example.com.crt".into(),
        key_file: "/certs/example.com.key".into(),
        expires_at: Some(Utc::now()),
        created_at: Utc::now(),
        sans: vec!["example.com".into()],
        source: "manual".into(),
        acme_dns_provider: None,
        acme_account_id: None,
        issued_at: 0,
    };

    // Create
    db::upsert_cert(&conn, &cert).unwrap();

    // List
    let certs = db::list_certs(&conn).unwrap();
    assert_eq!(certs.len(), 1);
    assert_eq!(certs[0].domain, "example.com");

    // Delete
    db::delete_cert(&conn, "example.com").unwrap();
    let certs = db::list_certs(&conn).unwrap();
    assert!(certs.is_empty());
}

/// admin_reload_indexes — after upsert, indexes reflect new data
#[test]
fn admin_reload_indexes() {
    let (_dir, conn) = temp_conn();

    // Insert site + domain
    let site = Site {
        name: "reload-test".into(),
        backend: "http://127.0.0.1:8080".into(),
        enabled: true,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        host_mode: HostMode::Passthrough,
        host_custom: None,
        domain_count: 0,
    };
    db::upsert_site(&conn, &site).unwrap();

    let domain = Domain {
        domain: "reload.example.com".into(),
        site_name: "reload-test".into(),
        enabled: true,
        auto_issue: false,
        dns_provider: None,
        created_at: Utc::now(),
    };
    db::upsert_domain(&conn, &domain).unwrap();

    // Build indexes
    let sites = db::list_sites(&conn).unwrap();
    let domains = db::list_domains(&conn).unwrap();
    let indexes = pangolin_core::index::Indexes::build(sites, domains);

    // Look up
    let result = pangolin_core::index::lookup_site(&indexes, "reload.example.com");
    assert!(result.is_some());
    assert_eq!(result.unwrap().name, "reload-test");
}

/// admin_site_host_mode_backend — host_mode=backend is stored and retrieved
#[test]
fn admin_site_host_mode_backend() {
    let (_dir, conn) = temp_conn();

    let site = Site {
        name: "backend-site".into(),
        backend: "http://192.168.1.100:8080".into(),
        enabled: true,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        host_mode: HostMode::Backend,
        host_custom: None,
        domain_count: 0,
    };
    db::upsert_site(&conn, &site).unwrap();

    let sites = db::list_sites(&conn).unwrap();
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].host_mode, HostMode::Backend);
    assert!(sites[0].host_custom.is_none());
}

/// admin_site_host_mode_custom — host_mode=custom with host_custom value is stored
#[test]
fn admin_site_host_mode_custom() {
    let (_dir, conn) = temp_conn();

    let site = Site {
        name: "custom-site".into(),
        backend: "http://127.0.0.1:8080".into(),
        enabled: true,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        host_mode: HostMode::Custom,
        host_custom: Some("internal.example.com".into()),
        domain_count: 0,
    };
    db::upsert_site(&conn, &site).unwrap();

    let sites = db::list_sites(&conn).unwrap();
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].host_mode, HostMode::Custom);
    assert_eq!(
        sites[0].host_custom.as_deref(),
        Some("internal.example.com")
    );
}

/// admin_site_host_mode_passthrough — passthrough (default) round-trips
#[test]
fn admin_site_host_mode_passthrough() {
    let (_dir, conn) = temp_conn();

    let site = Site {
        name: "passthrough-site".into(),
        backend: "http://127.0.0.1:8080".into(),
        enabled: true,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        host_mode: HostMode::Passthrough,
        host_custom: None,
        domain_count: 0,
    };
    db::upsert_site(&conn, &site).unwrap();

    let sites = db::list_sites(&conn).unwrap();
    assert_eq!(sites[0].host_mode, HostMode::Passthrough);
}

/// admin_site_host_mode_update — updating host_mode persists correctly
#[test]
fn admin_site_host_mode_update() {
    let (_dir, conn) = temp_conn();

    let initial = Site {
        name: "update-site".into(),
        backend: "http://127.0.0.1:8080".into(),
        enabled: true,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        host_mode: HostMode::Passthrough,
        host_custom: None,
        domain_count: 0,
    };
    db::upsert_site(&conn, &initial).unwrap();

    let updated = Site {
        name: "update-site".into(),
        backend: "http://127.0.0.1:8080".into(),
        enabled: true,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        host_mode: HostMode::Custom,
        host_custom: Some("updated.example.com".into()),
        domain_count: 0,
    };
    db::upsert_site(&conn, &updated).unwrap();

    let sites = db::list_sites(&conn).unwrap();
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].host_mode, HostMode::Custom);
    assert_eq!(sites[0].host_custom.as_deref(), Some("updated.example.com"));
}
