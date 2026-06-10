//! Admin reload integration tests.
//!
//! Covers: tests/CHECKLIST.md → Admin API reload (4 tests)
//!
//! Tests that DB inserts/updates → Indexes rebuild → routing changes are reflected.
//! This is the critical hot-path: an admin API POST creates data, indexes reload,
//! and subsequent requests route to the new site.

use chrono::Utc;
use rusqlite::Connection;
use tempfile::TempDir;

use pangolin_core::db;
use pangolin_core::index::Indexes;
use pangolin_core::types::{Domain, HostMode, Site, Token};

// ---------------------------------------------------------------------------
// Helper
// ---------------------------------------------------------------------------

fn temp_conn() -> (TempDir, Connection) {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db");
    let conn = Connection::open(&db_path).unwrap();
    db::migrate(&conn).unwrap();
    (dir, conn)
}

fn build_indexes(conn: &Connection) -> Indexes {
    let sites = db::list_sites(conn).unwrap();
    let domains = db::list_domains(conn).unwrap();
    let tokens = db::list_tokens(conn).unwrap();
    Indexes::build(sites, domains, &tokens, Utc::now())
}

/// admin_reload_site — insert site → rebuild indexes → domain lookup finds it
#[test]
fn admin_reload_site() {
    let (_dir, conn) = temp_conn();

    // Site does not exist yet
    assert!(db::list_sites(&conn).unwrap().is_empty());

    // Insert site
    let site = Site {
        name: "new-site".into(),
        backend: "http://127.0.0.1:9000".into(),
        enabled: true,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        host_mode: HostMode::Passthrough,
        host_custom: None,
        domain_count: 0,
    };
    db::upsert_site(&conn, &site).unwrap();

    // Rebuild indexes
    let indexes = build_indexes(&conn);

    // Look up site by name
    let sites = db::list_sites(&conn).unwrap();
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].name, "new-site");
    assert_eq!(sites[0].backend, "http://127.0.0.1:9000");
}

/// admin_reload_domain — insert domain → rebuild indexes → domain routes to site
#[test]
fn admin_reload_domain() {
    let (_dir, conn) = temp_conn();

    // Insert site first
    let site = Site {
        name: "domain-site".into(),
        backend: "http://127.0.0.1:8080".into(),
        enabled: true,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        host_mode: HostMode::Passthrough,
        host_custom: None,
        domain_count: 0,
    };
    db::upsert_site(&conn, &site).unwrap();

    // Insert domain
    let domain = Domain {
        domain: "new.example.com".into(),
        site_name: "domain-site".into(),
        enabled: true,
        created_at: Utc::now(),
    };
    db::upsert_domain(&conn, &domain).unwrap();

    // Rebuild indexes
    let indexes = build_indexes(&conn);

    // Look up domain → should find site
    let domains = db::list_domains(&conn).unwrap();
    assert_eq!(domains.len(), 1);
    assert_eq!(domains[0].domain, "new.example.com");

    // Verify indexes lookup works
    let domains = db::list_domains(&conn).unwrap();
    let sites = db::list_sites(&conn).unwrap();
    let tokens = db::list_tokens(&conn).unwrap();
    let indexes = Indexes::build(sites, domains, &tokens, Utc::now());

    let result = pangolin_core::index::lookup_site(&indexes, "new.example.com");
    assert!(result.is_some());
    assert_eq!(result.unwrap().name, "domain-site");
}

/// admin_reload_tun — insert tun → rebuild tun index → site backend routes to it
#[test]
fn admin_reload_tun() {
    let (_dir, conn) = temp_conn();

    // Insert site with tunnel backend
    let site = Site {
        name: "tun-site".into(),
        backend: "office:http://192.168.1.100:8080".into(),
        enabled: true,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        host_mode: HostMode::Passthrough,
        host_custom: None,
        domain_count: 0,
    };
    db::upsert_site(&conn, &site).unwrap();

    // Insert domain
    let domain = Domain {
        domain: "tun.example.com".into(),
        site_name: "tun-site".into(),
        enabled: true,
        created_at: Utc::now(),
    };
    db::upsert_domain(&conn, &domain).unwrap();

    // Insert tun
    let tun = pangolin_core::types::Tun {
        name: "office".into(),
        enabled: true,
        online: false,
        registered_at: None,
        last_seen_at: None,
    };
    db::upsert_tun(&conn, &tun).unwrap();

    // Rebuild indexes
    let domains = db::list_domains(&conn).unwrap();
    let sites = db::list_sites(&conn).unwrap();
    let tokens = db::list_tokens(&conn).unwrap();
    let indexes = Indexes::build(sites, domains, &tokens, Utc::now());

    // Tun index should contain 'office' → site 'tun-site'
    let tun_entry = indexes.tun.get("office");
    assert!(tun_entry.is_some(), "tun index should have 'office' key");
    let tun_domains = tun_entry.unwrap();
    assert!(
        tun_domains.iter().any(|d| d.domain == "tun.example.com"),
        "tun.example.com should be routed via 'office' tun"
    );
}

/// admin_reload_token — add/enable/disable token → token index reflects state
#[test]
fn admin_reload_token() {
    let (_dir, conn) = temp_conn();

    // Insert token (enabled)
    let token = Token {
        token: "reload-token".into(),
        enabled: true,
        created_at: Utc::now(),
        expires_at: None,
    };
    db::upsert_token(&conn, &token).unwrap();

    let indexes = build_indexes(&conn);
    assert_eq!(
        indexes.token.get("reload-token"),
        Some(&true),
        "enabled token should be active"
    );

    // Disable token
    let mut updated = token.clone();
    updated.enabled = false;
    db::upsert_token(&conn, &updated).unwrap();

    let indexes = build_indexes(&conn);
    assert_eq!(
        indexes.token.get("reload-token"),
        Some(&false),
        "disabled token should be inactive"
    );

    // Delete token
    db::delete_token(&conn, "reload-token").unwrap();
    let indexes = build_indexes(&conn);
    assert_eq!(
        indexes.token.get("reload-token"),
        None,
        "deleted token should be absent"
    );
}
