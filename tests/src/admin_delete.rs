//! DELETE API integration tests.
//!
//! Covers: tests/E2E_PLAN.md → DELETE API (4 tests)
//!
//! Tests that DELETE removes resources from DB.
//! Direct DB-level tests (the HTTP routing is tested in admin_api).

use chrono::Utc;
use rusqlite::Connection;
use tempfile::TempDir;

use pangolin_core::db;
use pangolin_core::types::{Domain, HostMode, Site, Token, Tun};

fn temp_conn() -> (TempDir, Connection) {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db");
    let mut conn = Connection::open(&db_path).unwrap();
    db::migrate(&mut conn).unwrap();
    (dir, conn)
}

/// admin_delete_site — delete_site removes site from DB
#[test]
fn admin_delete_site() {
    let (_dir, conn) = temp_conn();

    let site = Site {
        name: "to-delete".into(),
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
    assert_eq!(sites.len(), 1);

    db::delete_site(&conn, "to-delete").unwrap();

    let sites = db::list_sites(&conn).unwrap();
    assert!(sites.is_empty(), "site should be deleted");
}

/// admin_delete_domain — delete_domain removes domain from DB
#[test]
fn admin_delete_domain() {
    let (_dir, conn) = temp_conn();

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

    let domain = Domain {
        domain: "delete-me.example.com".into(),
        site_name: "my-site".into(),
        enabled: true,
        created_at: Utc::now(),
    };
    db::upsert_domain(&conn, &domain).unwrap();

    let domains = db::list_domains(&conn).unwrap();
    assert_eq!(domains.len(), 1);

    db::delete_domain(&conn, "delete-me.example.com").unwrap();

    let domains = db::list_domains(&conn).unwrap();
    assert!(domains.is_empty(), "domain should be deleted");
}

/// admin_delete_tun — delete_tun removes tun from DB
#[test]
fn admin_delete_tun() {
    let (_dir, conn) = temp_conn();

    let tun = Tun {
        name: "del-tun".into(),
        enabled: true,
        online: false,
        registered_at: None,
        last_seen_at: None,
    };
    db::upsert_tun(&conn, &tun).unwrap();

    let tuns = db::list_tuns(&conn).unwrap();
    assert_eq!(tuns.len(), 1);

    db::delete_tun(&conn, "del-tun").unwrap();

    let tuns = db::list_tuns(&conn).unwrap();
    assert!(tuns.is_empty(), "tun should be deleted");
}

/// admin_delete_token — delete_token removes token from DB
#[test]
fn admin_delete_token() {
    let (_dir, conn) = temp_conn();

    let token = Token {
        token: "del-token-xyz".into(),
        enabled: true,
        created_at: Utc::now(),
        expires_at: None,
    };
    db::upsert_token(&conn, &token).unwrap();

    let tokens = db::list_tokens(&conn).unwrap();
    assert_eq!(tokens.len(), 1);

    db::delete_token(&conn, "del-token-xyz").unwrap();

    let tokens = db::list_tokens(&conn).unwrap();
    assert!(tokens.is_empty(), "token should be deleted");
}
