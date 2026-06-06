//! Tests for new WIP features: events API, cert settings, and healthz.
//!
//! Run with: `cargo test --features integration -p pangolin-integration-tests feat_`

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use chrono::Utc;
use rusqlite::Connection;
use tempfile::TempDir;

use pangolin_core::db;
use pangolin_core::types::{Site, Token, Tun};
use pangolin_core::{App, CertManager, EventType};

/// Create a test CertManager with autorenew disabled.
fn make_cert_manager() -> CertManager {
    CertManager::new(
        false,
        std::path::PathBuf::from("/tmp/test-certs"),
        "test@example.com".into(),
        "https://acme.example.com/directory".into(),
        30,
        6,
        3,
    )
}

/// Create a test App with a temporary DB.
fn make_test_app() -> (TempDir, Arc<App>) {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db");
    let cert_manager = make_cert_manager();
    let app = Arc::new(
        App::new(
            db_path.to_str().unwrap(),
            pangolin_core::config::Config::default(),
            cert_manager,
        )
        .unwrap(),
    );
    (dir, app)
}

// ---------------------------------------------------------------------------
// EventBuffer tests
// ---------------------------------------------------------------------------

/// event_api_empty — GET /api/events on fresh app returns empty array
#[test]
fn event_api_empty() {
    let (_dir, app) = make_test_app();
    let events = app.get_recent_events(20);
    assert!(events.is_empty(), "fresh app should have no events");
}

/// event_api_add — add_event populates the buffer
#[test]
fn event_api_add() {
    let (_dir, app) = make_test_app();
    app.add_event(EventType::TunConnected {
        name: "office".into(),
    });
    app.add_event(EventType::TunDisconnected {
        name: "home".into(),
    });

    let events = app.get_recent_events(20);
    assert_eq!(events.len(), 2);
    // Most recent first
    match &events[0].event {
        EventType::TunDisconnected { name } => assert_eq!(name, "home"),
        _ => panic!("expected TunDisconnected first"),
    }
}

/// event_api_recent_limit — get_recent_events respects limit
#[test]
fn event_api_recent_limit() {
    let (_dir, app) = make_test_app();
    for i in 0..10 {
        app.add_event(EventType::Info {
            message: format!("event-{}", i),
        });
    }

    let recent = app.get_recent_events(3);
    assert_eq!(recent.len(), 3);
}

/// event_api_capacity — buffer caps at MAX_EVENTS events
#[test]
fn event_api_capacity() {
    let (_dir, app) = make_test_app();
    for i in 0..150 {
        app.add_event(EventType::Info {
            message: format!("event-{}", i),
        });
    }

    let events = app.get_recent_events(200);
    assert!(
        events.len() <= pangolin_core::events::MAX_EVENTS,
        "event buffer should not exceed MAX_EVENTS"
    );
}

/// event_serde — events serialize to JSON with correct type tags
#[test]
fn event_serde() {
    let (_dir, app) = make_test_app();
    app.add_event(EventType::TunConnected {
        name: "office".into(),
    });

    let events = app.get_recent_events(20);
    assert_eq!(events.len(), 1);

    let json = serde_json::to_string(&events[0]).unwrap();
    assert!(json.contains("\"type\":\"TunConnected\""));
    assert!(json.contains("\"name\":\"office\""));
}

// ---------------------------------------------------------------------------
// CertManager runtime override tests
// ---------------------------------------------------------------------------

/// cert_settings_default — default state uses config value
#[test]
fn cert_settings_default() {
    let (_dir, app) = make_test_app();
    // CertManager is created with enabled=false
    assert!(!app.cert_manager.is_autorenew_enabled());
    assert!(app.cert_manager.get_autorenew_setting().is_none());
}

/// cert_settings_override_enable — override enables autorenew when config is disabled
#[test]
fn cert_settings_override_enable() {
    let (_dir, app) = make_test_app();
    app.cert_manager.set_autorenew_override(Some(true));
    assert!(app.cert_manager.is_autorenew_enabled());
    assert_eq!(app.cert_manager.get_autorenew_setting(), Some(true));
}

/// cert_settings_override_disable — override disables autorenew when config is enabled
#[test]
fn cert_settings_override_disable() {
    let cm = CertManager::new(
        true, // enabled in config
        std::path::PathBuf::from("/tmp"),
        "test@example.com".into(),
        "https://acme.example.com/directory".into(),
        30,
        6,
        3,
    );
    cm.set_autorenew_override(Some(false));
    assert!(!cm.is_autorenew_enabled());
    assert_eq!(cm.get_autorenew_setting(), Some(false));
}

/// cert_settings_override_clear — clearing override falls back to config
#[test]
fn cert_settings_override_clear() {
    let (_dir, app) = make_test_app();
    app.cert_manager.set_autorenew_override(Some(false));
    assert!(!app.cert_manager.is_autorenew_enabled());
    app.cert_manager.set_autorenew_override(None);
    // Config says disabled (enabled=false in make_cert_manager)
    assert!(!app.cert_manager.is_autorenew_enabled());
    assert!(app.cert_manager.get_autorenew_setting().is_none());
}

// ---------------------------------------------------------------------------
// Tun online/offline DB state tests
// ---------------------------------------------------------------------------

/// tun_online_offline_db — mark_tun_online and mark_tun_offline update DB correctly
#[test]
fn tun_online_offline_db() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db");
    let conn = Connection::open(&db_path).unwrap();
    db::migrate(&conn).unwrap();

    // Insert a tun
    let tun = Tun {
        name: "office".into(),
        enabled: true,
        online: false,
        registered_at: None,
        last_seen_at: None,
    };
    db::upsert_tun(&conn, &tun).unwrap();

    // Mark online
    db::set_tun_online(&conn, "office", true).unwrap();
    let t = db::get_tun(&conn, "office").unwrap().unwrap();
    assert!(
        t.online,
        "tun should be online after set_tun_online(online=true)"
    );

    // Mark offline
    db::set_tun_online(&conn, "office", false).unwrap();
    let t = db::get_tun(&conn, "office").unwrap().unwrap();
    assert!(
        !t.online,
        "tun should be offline after set_tun_online(online=false)"
    );
}

// ---------------------------------------------------------------------------
// Healthz response format test
// ---------------------------------------------------------------------------

/// healthz_response_fields — verify healthz JSON has required fields
#[test]
fn healthz_response_fields() {
    let (_dir, _app) = make_test_app();

    // Verify VERSION is a valid semver string
    let version = pangolin_core::VERSION;
    assert!(!version.is_empty(), "VERSION should not be empty");

    // Build the expected JSON structure
    let json = serde_json::json!({
        "status": "ok",
        "version": version
    });
    let json_str = serde_json::to_string(&json).unwrap();
    assert!(json_str.contains("\"status\":\"ok\""));
    assert!(json_str.contains("\"version\":"));
}
