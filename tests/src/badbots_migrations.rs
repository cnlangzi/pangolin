//! Migration regression tests for the badbots / knownbots upgrade
//! (issue #151).
//!
//! Two migrations ship with the upgrade, each carrying one topic:
//!
//!   * **V13** drops the operator-facing list-refresh-interval column
//!     from the `badbots_config` singleton. The setting never decided
//!     when a list was downloaded — the publisher's own refresh floor
//!     does, and that floor is 24 hours now — so the knob controls
//!     nothing the operator can act on.
//!   * **V14** rewrites the per-site bot list for the crawlers the
//!     knownbots registry no longer has: `applebot-extended` becomes
//!     `applebot`, and the rows naming `google-extended` and
//!     `cloudflare-alwaysonline` are deleted.
//!
//! The tests are hermetic — they drive refinery against a throwaway
//! SQLite file and need neither a binary nor the network, so they run
//! in the day-to-day `cargo test --workspace` / `make lint` cycle.

use rusqlite::Connection;
use tempfile::TempDir;

use pangolin_core::db;
use pangolin_core::embedded_migrations::run_migrations_up_to;

/// A database in a temp dir, migrated up to `version`.
fn conn_up_to(version: i32) -> (TempDir, Connection) {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("test.db");
    let mut conn = Connection::open(&db_path).expect("open");
    run_migrations_up_to(&mut conn, version).expect("migrate up to the version");
    (dir, conn)
}

/// The column names of a table.
fn columns(conn: &Connection, table: &str) -> Vec<String> {
    conn.prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
        .expect("prepare")
        .query_map([], |r| r.get::<_, String>(0))
        .expect("query")
        .map(|r| r.expect("row"))
        .collect()
}

/// The bot ids a site lists, sorted.
fn listed_bots(conn: &Connection, site: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT bot_name FROM site_rate_limit_bots WHERE site_name = ?1 ORDER BY bot_name")
        .expect("prepare");
    stmt.query_map([site], |r| r.get::<_, String>(0))
        .expect("query")
        .map(|r| r.expect("row"))
        .collect()
}

/// The refresh-interval column is gone, and the rest of the singleton
/// row survived the drop (R7).
///
/// The drop is a schema change on a table that holds the operator's
/// credentials, so the row must come through it intact: `enabled`,
/// `evidence_policy`, both API keys and `updated_at` all read back.
#[test]
fn the_refresh_interval_column_is_dropped_and_the_row_survives() {
    let (_dir, mut conn) = conn_up_to(12);

    // Before V13 the column is there.
    assert!(
        columns(&conn, "badbots_config")
            .iter()
            .any(|c| c == "feed_refresh_interval_secs"),
        "V12 must still carry the column"
    );

    // Give the row something to lose.
    conn.execute(
        "UPDATE badbots_config
            SET enabled = 0,
                evidence_policy = 'strict',
                abuseipdb_api_key = 'key-a',
                crowdsec_api_key = 'key-c',
                updated_at = '2026-01-02T03:04:05Z'
          WHERE id = 1",
        [],
    )
    .expect("seed the row");

    // The upgrade runs the full migration set.
    db::migrate(&mut conn).expect("migrate");

    // The column is gone …
    let cols = columns(&conn, "badbots_config");
    assert!(
        !cols.iter().any(|c| c == "feed_refresh_interval_secs"),
        "V13 must drop the column: {cols:?}"
    );

    // … and every other column of the row reads back unchanged.
    let cfg = db::get_badbots_config(&conn).expect("the row is still readable");
    assert!(!cfg.enabled);
    assert_eq!(
        cfg.evidence_policy,
        pangolin_core::BadbotsEvidencePolicy::Strict
    );
    assert_eq!(cfg.abuseipdb_api_key.as_deref(), Some("key-a"));
    assert_eq!(cfg.crowdsec_api_key.as_deref(), Some("key-c"));
    assert_eq!(
        cfg.updated_at,
        chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
            .expect("rfc3339")
            .with_timezone(&chrono::Utc),
        "the update timestamp must survive the drop"
    );
}

/// The removed crawler ids are rewritten (R8).
///
/// `applebot-extended` is gone from the registry, but a request that
/// carries `Applebot-Extended` still matches the marker `Applebot`
/// (a hyphen is a word boundary) and is now identified as `applebot`.
/// A site that deliberately listed `applebot-extended` would silently
/// lose that intent, so the row is rewritten. The two ids with no
/// successor are deleted — they match no request any more.
#[test]
fn the_removed_crawler_ids_are_rewritten() {
    let (_dir, mut conn) = conn_up_to(13);

    conn.execute(
        "INSERT INTO site_rate_limit_bots (site_name, bot_name)
         VALUES ('example.com', 'applebot-extended'),
                ('example.com', 'google-extended'),
                ('example.com', 'cloudflare-alwaysonline')",
        [],
    )
    .expect("seed the bot list");

    db::migrate(&mut conn).expect("migrate");

    assert_eq!(
        listed_bots(&conn, "example.com"),
        vec!["applebot".to_string()],
        "the applebot-extended row must become an applebot row, and the two ids \
         with no successor must be gone"
    );
}

/// A site that lists both `applebot` and `applebot-extended` ends up
/// with one `applebot` row (R9).
///
/// The rewrite must not violate the primary key `(site_name,
/// bot_name)`: the migration has to succeed, and the site must not end
/// up with two rows for the same crawler.
#[test]
fn a_site_that_lists_both_ids_ends_up_with_one_row() {
    let (_dir, mut conn) = conn_up_to(13);

    conn.execute(
        "INSERT INTO site_rate_limit_bots (site_name, bot_name)
         VALUES ('example.com', 'applebot'),
                ('example.com', 'applebot-extended')",
        [],
    )
    .expect("seed the bot list");

    // The migration must not fail on the primary key.
    db::migrate(&mut conn).expect("migrate");

    assert_eq!(
        listed_bots(&conn, "example.com"),
        vec!["applebot".to_string()],
        "exactly one applebot row must remain"
    );
}
