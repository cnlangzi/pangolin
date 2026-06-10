//! Reload indexes integration tests.
//!
//! Covers: tests/E2E_PLAN.md → reload_indexes_triggered
//! Directly tests App::reload_indexes() method behavior.

use chrono::Utc;
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;

use pangolin_core::db;
use pangolin_core::types::{Domain, HostMode, Site, Token};
use pangolin_core::{App, CertManager};

fn make_cert_manager(cert_dir: &PathBuf) -> CertManager {
    CertManager::new(
        false,
        cert_dir.clone(),
        String::new(),
        String::new(),
        30,
        24,
        3,
        "ecdsa".into(),
    )
}

fn make_test_app(dir: &tempfile::TempDir) -> Arc<App> {
    let db_path = dir.path().join("test.db");
    let conn = Connection::open(&db_path).unwrap();
    db::migrate(&conn).unwrap();

    let cert_dir = dir.path().join("certs");
    let cert_manager = make_cert_manager(&cert_dir);

    Arc::new(
        App::new(
            db_path.to_str().unwrap(),
            pangolin_core::config::Config::default(),
            cert_manager,
        )
        .unwrap(),
    )
}

/// reload_indexes_triggered — after upsert_site, App::reload_indexes called
/// and new site is queryable in the index.
#[tokio::test]
async fn reload_indexes_triggered() {
    let dir = TempDir::new().unwrap();
    let app = make_test_app(&dir);

    // Initial state: no sites
    {
        let idx = app.indexes.read().await;
        assert!(pangolin_core::index::lookup_site(&idx, "new-site.com").is_none());
    }

    // Insert site + domain via DB
    let site = Site {
        name: "new-site".into(),
        backend: "http://127.0.0.1:8080".into(),
        enabled: true,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        host_mode: HostMode::Passthrough,
        host_custom: None,
        domain_count: 0,
    };
    db::upsert_site(&*app.db.lock().await, &site).unwrap();

    let domain = Domain {
        domain: "new-site.example.com".into(),
        site_name: "new-site".into(),
        enabled: true,
        created_at: Utc::now(),
    };
    db::upsert_domain(&*app.db.lock().await, &domain).unwrap();

    // Call reload_indexes()
    app.reload_indexes().await;

    // Verify site is now routable by domain
    {
        let idx = app.indexes.read().await;
        let found = pangolin_core::index::lookup_site(&idx, "new-site.example.com");
        assert!(
            found.is_some(),
            "site should be routable after reload_indexes"
        );
        assert_eq!(found.unwrap().name, "new-site");
    }
}

/// reload_indexes_domain_triggers_routing — after upsert_domain,
/// reload_indexes makes the domain routable.
#[tokio::test]
async fn reload_indexes_domain_triggers_routing() {
    let dir = TempDir::new().unwrap();
    let app = make_test_app(&dir);

    // Insert site first
    let site = Site {
        name: "routing-site".into(),
        backend: "http://127.0.0.1:8080".into(),
        enabled: true,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        host_mode: HostMode::Passthrough,
        host_custom: None,
        domain_count: 0,
    };
    db::upsert_site(&*app.db.lock().await, &site).unwrap();
    app.reload_indexes().await;

    // Initially, no domain
    {
        let idx = app.indexes.read().await;
        assert!(pangolin_core::index::lookup_site(&idx, "mydomain.example.com").is_none());
    }

    // Add domain
    let domain = Domain {
        domain: "mydomain.example.com".into(),
        site_name: "routing-site".into(),
        enabled: true,
        created_at: Utc::now(),
    };
    db::upsert_domain(&*app.db.lock().await, &domain).unwrap();
    app.reload_indexes().await;

    // Now routable
    {
        let idx = app.indexes.read().await;
        let found = pangolin_core::index::lookup_site(&idx, "mydomain.example.com");
        assert!(found.is_some(), "domain should be routable after reload");
        assert_eq!(found.unwrap().name, "routing-site");
    }
}

/// reload_indexes_token_affects_active_state — after disabling token,
/// reload_indexes reflects the change in tokenIndex.
#[tokio::test]
async fn reload_indexes_token_affects_active_state() {
    let dir = TempDir::new().unwrap();
    let app = make_test_app(&dir);

    // Insert enabled token
    let token = Token {
        token: "test-reload-token".into(),
        enabled: true,
        created_at: Utc::now(),
        expires_at: None,
    };
    db::upsert_token(&*app.db.lock().await, &token).unwrap();
    app.reload_indexes().await;

    {
        let idx = app.indexes.read().await;
        assert_eq!(idx.token.get("test-reload-token"), Some(&true));
    }

    // Disable token
    let mut disabled = token.clone();
    disabled.enabled = false;
    db::upsert_token(&*app.db.lock().await, &disabled).unwrap();
    app.reload_indexes().await;

    {
        let idx = app.indexes.read().await;
        assert_eq!(idx.token.get("test-reload-token"), Some(&false));
    }
}

/// reload_indexes_no_change_is_idempotent — calling reload_indexes
/// when nothing changed is safe (no panic, index unchanged).
#[tokio::test]
async fn reload_indexes_no_change_is_idempotent() {
    let dir = TempDir::new().unwrap();
    let app = make_test_app(&dir);

    let site = Site {
        name: "stable-site".into(),
        backend: "http://127.0.0.1:9000".into(),
        enabled: true,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        host_mode: HostMode::Passthrough,
        host_custom: None,
        domain_count: 0,
    };
    db::upsert_site(&*app.db.lock().await, &site).unwrap();
    app.reload_indexes().await;

    // Get first index state
    let idx1 = app.indexes.read().await;
    let site1_name =
        pangolin_core::index::lookup_site(&idx1, "stable-site.com").map(|s| s.name.clone());
    let token_count = idx1.token.len();
    drop(idx1);

    // Reload again — nothing changed
    app.reload_indexes().await;

    let idx2 = app.indexes.read().await;
    let site2_name =
        pangolin_core::index::lookup_site(&idx2, "stable-site.com").map(|s| s.name.clone());
    assert_eq!(idx2.token.len(), token_count);
    assert_eq!(site1_name, site2_name, "index should be unchanged");
}
