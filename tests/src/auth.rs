//! Tunnel auth integration tests.
//!
//! v2: auth is no longer an in-memory `token` index. The WS server
//! runs a single `auth_tun(name, token)` SQL query against the
//! `tun` table. These tests pin that path: matching (name, token),
//! mismatched name, mismatched token, and disabled row.

use rusqlite::Connection;
use tempfile::TempDir;

use pangolin_core::db;
use pangolin_core::types::Tun;

fn fresh() -> (TempDir, Connection) {
    let dir = tempfile::tempdir().expect("tempdir");
    let conn = db::open(dir.path().join("t.sqlite")).expect("open");
    let mut c = conn;
    db::migrate(&mut c).expect("migrate");
    (dir, c)
}

fn insert(conn: &Connection, name: &str, token: &str, enabled: bool) {
    db::upsert_tun(
        conn,
        &Tun {
            name: name.into(),
            token: Some(token.into()),
            enabled,
            online: false,
            registered_at: None,
            last_seen_at: None,
            expires_at: None,
        },
    )
    .expect("upsert_tun");
}

/// auth_match — (name, token) match a row → enabled flag returned
#[test]
fn auth_match() {
    let (_d, c) = fresh();
    insert(&c, "office", "tok-abc", true);
    let r = db::auth_tun(&c, "office", "tok-abc").unwrap();
    assert_eq!(r.map(|(e, _)| e), Some(true));
}

/// auth_wrong_token — wrong token → no row
#[test]
fn auth_wrong_token() {
    let (_d, c) = fresh();
    insert(&c, "office", "tok-abc", true);
    assert!(db::auth_tun(&c, "office", "wrong").unwrap().is_none());
}

/// auth_wrong_name — wrong name → no row
#[test]
fn auth_wrong_name() {
    let (_d, c) = fresh();
    insert(&c, "office", "tok-abc", true);
    assert!(db::auth_tun(&c, "nope", "tok-abc").unwrap().is_none());
}

/// auth_disabled_row — (name, token) match but enabled=0 → row
/// surfaces as disabled. The WS server must reject this even
/// though the credential pair matched, because an operator
/// deliberately turned the row off.
#[test]
fn auth_disabled_row() {
    let (_d, c) = fresh();
    insert(&c, "office", "tok-abc", false);
    let (enabled, _) = db::auth_tun(&c, "office", "tok-abc").unwrap().unwrap();
    assert!(!enabled);
}

/// auth_empty_table — no rows at all → None
#[test]
fn auth_empty_table() {
    let (_d, c) = fresh();
    assert!(db::auth_tun(&c, "office", "tok-abc").unwrap().is_none());
}
