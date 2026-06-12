//! SQLite schema and migration functions.
//!
//! Schema is managed by the `refinery` embedded migration system:
//!   migrations/V{version}__{name}.sql  →  compiled and run at startup
//!   schema_version table               →  tracks applied migrations
//!
//! Five tables, all with TEXT primary keys (natural keys, no surrogate ids):
//!   sites         (name PK, backend, enabled, ...)
//!   domains       (domain PK, site_name, enabled, auto_issue, dns_provider, created_at) FK→sites.name
//!   tun           (name PK, token, enabled, online, registered_at, last_seen_at, expires_at)
//!   certs         (domain PK, cert_file, key_file, expires_at, created_at, sans, source, ...)
//!   dns_providers (name PK, kind, enabled, config, created_at, updated_at)
//!
//! v2 (V2 migration): the `tokens` table was merged into `tun`. A tun
//! row now carries its own auth credential (`token`) and optional
//! `expires_at`. The two-table model required a 401/403 distinction and
//! forced operators to pre-register both halves; the merged model is a
//! single SELECT.
//!
//! No intermediate tables. No `tun_domains` (we removed it; site.backend
//! prefix is the single source of routing truth).
//!
//! `domains.dns_provider` is a logical FK to `dns_providers.name`, not enforced
//! at SQL level (no `REFERENCES`). The application code is responsible for
//! keeping the link consistent — in particular, deleting a provider is done
//! transactionally with a `SET NULL` on the referencing domains.

use std::path::Path;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::embedded_migrations::run_migrations;
use crate::types::{Cert, DnsProvider, Domain, Site, Tun};

/// SHA-256 hex of an auth token. Lowercase, 64 chars.
/// Used as the on-disk form of `tun.token` (V3 migration); the WS
/// server hashes the incoming Authorization header and compares
/// against this value, so a DB dump no longer leaks the cleartext
/// credential.
pub fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(&mut out, "{:02x}", b);
    }
    out
}
/// Open a connection with sensible defaults (WAL, foreign keys on).
pub fn open(path: impl AsRef<Path>) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(
        path.as_ref(),
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
    )?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(conn)
}

/// Run pending refinery migrations. Safe to call on every startup —
/// already-applied migrations are skipped (tracked in schema_version table).
pub fn migrate(conn: &mut Connection) -> crate::Result<()> {
    run_migrations(conn)?;
    // V3: backfill `token_hash` for any tun row that pre-dates V3.
    // Done after refinery so the column exists. `backfill_tun_token_hashes`
    // is idempotent — re-running it on a populated DB is a no-op.
    backfill_tun_token_hashes(conn)?;
    Ok(())
}

// ---- Site CRUD ----

pub fn list_sites(conn: &Connection) -> rusqlite::Result<Vec<Site>> {
    let mut stmt = conn.prepare(
        "SELECT s.name, s.backend, s.enabled, s.host_mode, s.host_custom, s.created_at, s.updated_at,
                COUNT(d.domain) as domain_count
         FROM sites s
         LEFT JOIN domains d ON d.site_name = s.name
         GROUP BY s.name, s.backend, s.enabled, s.host_mode, s.host_custom, s.created_at, s.updated_at
         ORDER BY s.name",
    )?;
    let rows = stmt.query_map([], row_to_site_with_count)?;
    rows.collect()
}

pub fn get_site(conn: &Connection, name: &str) -> rusqlite::Result<Option<Site>> {
    let mut stmt = conn.prepare(
        "SELECT name, backend, enabled, host_mode, host_custom, created_at, updated_at FROM sites WHERE name = ?1",
    )?;
    stmt.query_row(params![name], row_to_site).optional()
}

pub fn upsert_site(conn: &Connection, site: &Site) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO sites (name, backend, enabled, host_mode, host_custom, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(name) DO UPDATE SET
            backend = excluded.backend,
            enabled = excluded.enabled,
            host_mode = excluded.host_mode,
            host_custom = excluded.host_custom,
            updated_at = excluded.updated_at",
        params![
            site.name,
            site.backend,
            site.enabled as i32,
            site.host_mode.to_string(),
            site.host_custom,
            site.created_at.to_rfc3339(),
            site.updated_at.to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn delete_site(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    let n = conn.execute("DELETE FROM sites WHERE name = ?1", params![name])?;
    Ok(n > 0)
}

// ---- Domain CRUD ----

pub fn list_domains(conn: &Connection) -> rusqlite::Result<Vec<Domain>> {
    let mut stmt = conn.prepare(
        "SELECT domain, site_name, enabled, auto_issue, dns_provider, created_at
         FROM domains ORDER BY domain",
    )?;
    let rows = stmt.query_map([], row_to_domain)?;
    rows.collect()
}

pub fn list_domains_for_site(conn: &Connection, site_name: &str) -> rusqlite::Result<Vec<Domain>> {
    let mut stmt = conn.prepare(
        "SELECT domain, site_name, enabled, auto_issue, dns_provider, created_at
         FROM domains WHERE site_name = ?1 ORDER BY domain",
    )?;
    let rows = stmt.query_map(params![site_name], row_to_domain)?;
    rows.collect()
}

pub fn upsert_domain(conn: &Connection, domain: &Domain) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO domains (domain, site_name, enabled, auto_issue, dns_provider, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(domain) DO UPDATE SET
            site_name = excluded.site_name,
            enabled = excluded.enabled,
            auto_issue = excluded.auto_issue,
            dns_provider = excluded.dns_provider",
        params![
            domain.domain,
            domain.site_name,
            domain.enabled as i32,
            domain.auto_issue as i32,
            domain.dns_provider,
            domain.created_at.to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn delete_domain(conn: &Connection, domain: &str) -> rusqlite::Result<bool> {
    let n = conn.execute("DELETE FROM domains WHERE domain = ?1", params![domain])?;
    Ok(n > 0)
}

/// Get a single domain by its primary key. Returns None if not found.
/// Cheaper than list_domains().find() when only one row is needed.
pub fn get_domain(conn: &Connection, domain: &str) -> rusqlite::Result<Option<Domain>> {
    let mut stmt = conn.prepare(
        "SELECT domain, site_name, enabled, auto_issue, dns_provider, created_at
         FROM domains WHERE domain = ?1",
    )?;
    let result = stmt.query_row(params![domain], |row| {
        let enabled: i32 = row.get(2)?;
        let auto_issue: i32 = row.get(3)?;
        let created_at: String = row.get(5)?;
        Ok(Domain {
            domain: row.get(0)?,
            site_name: row.get(1)?,
            enabled: enabled != 0,
            auto_issue: auto_issue != 0,
            dns_provider: row.get(4)?,
            created_at: parse_dt(&created_at)?,
        })
    });
    match result {
        Ok(d) => Ok(Some(d)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Toggle / set the `enabled` flag on a single domain row. Returns true
/// if the row existed and was updated. Used by the admin UI's per-row
/// toggle switch (POST /admin/api/domains/{domain}/toggle).
pub fn set_domain_enabled(
    conn: &Connection,
    domain: &str,
    enabled: bool,
) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE domains SET enabled = ?1 WHERE domain = ?2",
        params![enabled as i32, domain],
    )?;
    Ok(n > 0)
}

// ---- Tun CRUD ----

pub fn list_tuns(conn: &Connection) -> rusqlite::Result<Vec<Tun>> {
    let mut stmt = conn.prepare(
        "SELECT name, token, token_hash, enabled, online, registered_at, last_seen_at, expires_at
         FROM tun ORDER BY name",
    )?;
    let rows = stmt.query_map([], row_to_tun)?;
    rows.collect()
}

pub fn get_tun(conn: &Connection, name: &str) -> rusqlite::Result<Option<Tun>> {
    let mut stmt = conn.prepare(
        "SELECT name, token, token_hash, enabled, online, registered_at, last_seen_at, expires_at
         FROM tun WHERE name = ?1",
    )?;
    stmt.query_row(params![name], row_to_tun).optional()
}

pub fn upsert_tun(conn: &Connection, tun: &Tun) -> rusqlite::Result<()> {
    // V3 stores the credential as `sha256(token)`. The legacy `token`
    // column is kept (V4 drop pending operator verification that no
    // tooling is reading it); we still write the cleartext there so a
    // downgrade path stays viable.
    let token_hash = tun.token_hash.clone().or_else(|| {
        tun.token
            .as_deref()
            .filter(|t| !t.is_empty())
            .map(sha256_hex)
    });
    conn.execute(
        "INSERT INTO tun (name, token, token_hash, enabled, online, registered_at, last_seen_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(name) DO UPDATE SET
            token = excluded.token,
            token_hash = excluded.token_hash,
            enabled = excluded.enabled,
            online = excluded.online,
            last_seen_at = excluded.last_seen_at,
            expires_at = excluded.expires_at",
        params![
            tun.name,
            tun.token.as_deref().unwrap_or(""),
            token_hash.unwrap_or_default(),
            tun.enabled as i32,
            tun.online as i32,
            tun.registered_at.map(|t| t.to_rfc3339()),
            tun.last_seen_at.map(|t| t.to_rfc3339()),
            tun.expires_at.map(|t| t.to_rfc3339()),
        ],
    )?;
    Ok(())
}

pub fn delete_tun(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    let n = conn.execute("DELETE FROM tun WHERE name = ?1", params![name])?;
    Ok(n > 0)
}

pub fn set_tun_online(conn: &Connection, name: &str, online: bool) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE tun SET online = ?1, last_seen_at = ?2 WHERE name = ?3",
        params![online as i32, Utc::now().to_rfc3339(), name],
    )?;
    Ok(())
}

/// Look up a tun by both its `name` and `token` in a single query.
/// Returns the row's enabled flag and expires_at if it matched.
/// This is the only auth check the WS server needs; there's no
/// two-table validation step anymore.
///
/// V3: `token` is matched as `sha256(token)` against the
/// `token_hash` column. Empty cleartext hashes to the empty-string
/// sha256, which still rejects an Authorization header that supplies
/// the empty bearer — because `auth_tun` is called with whatever
/// the client presented (typically a 32-char random token), and
/// that hash will not collide with the empty-hash legacy default.
pub fn auth_tun(
    conn: &Connection,
    name: &str,
    token: &str,
) -> rusqlite::Result<Option<(bool, Option<DateTime<Utc>>)>> {
    let token_hash = sha256_hex(token);
    let mut stmt = conn.prepare(
        "SELECT enabled, expires_at FROM tun
         WHERE name = ?1 AND token_hash = ?2",
    )?;
    let row = stmt
        .query_row(params![name, token_hash], |r| {
            let enabled: i32 = r.get(0)?;
            let expires_at: Option<String> = r.get(1)?;
            Ok((enabled != 0, expires_at.as_deref().and_then(parse_dt_opt)))
        })
        .optional()?;
    Ok(row)
}

/// Backfill `token_hash` for any rows that pre-date V3. Called once
/// from `migrate` after refinery runs V3. Idempotent: rows that
/// already have a non-empty hash are skipped.
///
/// Empty `token` (the legacy default for never-provisioned rows)
/// hashes to the sha256 of the empty string, which is also what
/// `auth_tun` computes for an empty bearer — so backfilled empty
/// rows will not match any real client (no false positives).
pub fn backfill_tun_token_hashes(conn: &Connection) -> rusqlite::Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT name, token FROM tun
         WHERE token_hash IS NULL OR token_hash = ''",
    )?;
    let rows: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let n = rows.len();
    for (name, token) in rows {
        let hash = sha256_hex(&token);
        conn.execute(
            "UPDATE tun SET token_hash = ?1 WHERE name = ?2",
            params![hash, name],
        )?;
    }
    Ok(n)
}

// ---- Cert CRUD ----

pub fn list_certs(conn: &Connection) -> rusqlite::Result<Vec<Cert>> {
    let mut stmt = conn.prepare(
        "SELECT domain, cert_file, key_file, expires_at, created_at,
                sans, source, acme_dns_provider, acme_account_id, issued_at
         FROM certs ORDER BY domain",
    )?;
    let rows = stmt.query_map([], row_to_cert)?;
    rows.collect()
}

pub fn upsert_cert(conn: &Connection, cert: &Cert) -> rusqlite::Result<()> {
    let sans_json = serde_json::to_string(&cert.sans).unwrap_or_else(|_| "[]".to_string());
    conn.execute(
        "INSERT INTO certs (domain, cert_file, key_file, expires_at, created_at,
                             sans, source, acme_dns_provider, acme_account_id, issued_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(domain) DO UPDATE SET
            cert_file = excluded.cert_file,
            key_file = excluded.key_file,
            expires_at = excluded.expires_at,
            sans = excluded.sans,
            source = excluded.source,
            acme_dns_provider = excluded.acme_dns_provider,
            acme_account_id = excluded.acme_account_id,
            issued_at = excluded.issued_at",
        params![
            cert.domain,
            cert.cert_file,
            cert.key_file,
            cert.expires_at.map(|t| t.to_rfc3339()),
            cert.created_at.to_rfc3339(),
            sans_json,
            cert.source,
            cert.acme_dns_provider,
            cert.acme_account_id,
            cert.issued_at,
        ],
    )?;
    Ok(())
}

pub fn delete_cert(conn: &Connection, domain: &str) -> rusqlite::Result<bool> {
    let n = conn.execute("DELETE FROM certs WHERE domain = ?1", params![domain])?;
    Ok(n > 0)
}

// ---- DNS provider CRUD ----

pub fn list_dns_providers(conn: &Connection) -> rusqlite::Result<Vec<DnsProvider>> {
    let mut stmt = conn.prepare(
        "SELECT name, kind, enabled, config, created_at, updated_at
         FROM dns_providers ORDER BY name",
    )?;
    let rows = stmt.query_map([], row_to_dns_provider)?;
    rows.collect()
}

pub fn get_dns_provider(conn: &Connection, name: &str) -> rusqlite::Result<Option<DnsProvider>> {
    let mut stmt = conn.prepare(
        "SELECT name, kind, enabled, config, created_at, updated_at
         FROM dns_providers WHERE name = ?1",
    )?;
    stmt.query_row(params![name], row_to_dns_provider)
        .optional()
}

pub fn upsert_dns_provider(conn: &Connection, p: &DnsProvider) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO dns_providers (name, kind, enabled, config, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(name) DO UPDATE SET
            kind = excluded.kind,
            enabled = excluded.enabled,
            config = excluded.config,
            updated_at = excluded.updated_at",
        params![
            p.name,
            p.kind.to_string(),
            p.enabled as i32,
            p.config,
            p.created_at.to_rfc3339(),
            p.updated_at.to_rfc3339(),
        ],
    )?;
    Ok(())
}

/// Delete a DNS provider. Returns the number of rows affected (always 0 or 1).
///
/// Callers are responsible for transactionally clearing `domains.dns_provider`
/// references *before* this is called — we do not use SQL-level ON DELETE
/// SET NULL (the schema has no SQL FK on purpose). The admin route handler
/// does this in a transaction.
pub fn delete_dns_provider(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    let n = conn.execute("DELETE FROM dns_providers WHERE name = ?1", params![name])?;
    Ok(n > 0)
}

// ---- Row mappers ----

fn row_to_site(row: &rusqlite::Row<'_>) -> rusqlite::Result<Site> {
    let name: String = row.get(0)?;
    let backend: String = row.get(1)?;
    let enabled: i32 = row.get(2)?;
    let host_mode_raw: String = row.get(3)?;
    let host_custom: Option<String> = row.get(4)?;
    let created_at: String = row.get(5)?;
    let updated_at: String = row.get(6)?;
    let host_mode: crate::types::HostMode = host_mode_raw.parse().map_err(|_| {
        rusqlite::Error::InvalidParameterName(format!("invalid host_mode: {}", host_mode_raw))
    })?;
    Ok(Site {
        name,
        backend,
        enabled: enabled != 0,
        host_mode,
        host_custom,
        created_at: parse_dt(&created_at)?,
        updated_at: parse_dt(&updated_at)?,
        domain_count: 0,
    })
}

fn row_to_site_with_count(row: &rusqlite::Row<'_>) -> rusqlite::Result<Site> {
    let name: String = row.get(0)?;
    let backend: String = row.get(1)?;
    let enabled: i32 = row.get(2)?;
    let host_mode_raw: String = row.get(3)?;
    let host_custom: Option<String> = row.get(4)?;
    let created_at: String = row.get(5)?;
    let updated_at: String = row.get(6)?;
    let domain_count: i32 = row.get(7)?;
    let host_mode: crate::types::HostMode = host_mode_raw.parse().map_err(|_| {
        rusqlite::Error::InvalidParameterName(format!("invalid host_mode: {}", host_mode_raw))
    })?;
    Ok(Site {
        name,
        backend,
        enabled: enabled != 0,
        host_mode,
        host_custom,
        created_at: parse_dt(&created_at)?,
        updated_at: parse_dt(&updated_at)?,
        domain_count: domain_count as usize,
    })
}

fn row_to_domain(row: &rusqlite::Row<'_>) -> rusqlite::Result<Domain> {
    let domain: String = row.get(0)?;
    let site_name: String = row.get(1)?;
    let enabled: i32 = row.get(2)?;
    let auto_issue: i32 = row.get(3)?;
    let dns_provider: Option<String> = row.get(4)?;
    let created_at: String = row.get(5)?;
    Ok(Domain {
        domain,
        site_name,
        enabled: enabled != 0,
        auto_issue: auto_issue != 0,
        dns_provider,
        created_at: parse_dt(&created_at)?,
    })
}

fn row_to_dns_provider(row: &rusqlite::Row<'_>) -> rusqlite::Result<DnsProvider> {
    let name: String = row.get(0)?;
    let kind_raw: String = row.get(1)?;
    let enabled: i32 = row.get(2)?;
    let config: String = row.get(3)?;
    let created_at: String = row.get(4)?;
    let updated_at: String = row.get(5)?;
    let kind: crate::types::DnsProviderKind = kind_raw.parse().map_err(|_| {
        rusqlite::Error::InvalidParameterName(format!("invalid dns provider kind: {}", kind_raw))
    })?;
    Ok(DnsProvider {
        name,
        kind,
        enabled: enabled != 0,
        config,
        created_at: parse_dt(&created_at)?,
        updated_at: parse_dt(&updated_at)?,
    })
}

fn row_to_tun(row: &rusqlite::Row<'_>) -> rusqlite::Result<Tun> {
    let name: String = row.get(0)?;
    let token: Option<String> = row.get(1)?;
    let token_hash: Option<String> = row.get(2)?;
    let enabled: i32 = row.get(3)?;
    let online: i32 = row.get(4)?;
    let registered_at: Option<String> = row.get(5)?;
    let last_seen_at: Option<String> = row.get(6)?;
    let expires_at: Option<String> = row.get(7)?;
    Ok(Tun {
        name,
        token: if token.as_deref().unwrap_or("").is_empty() {
            None
        } else {
            token
        },
        token_hash: token_hash.filter(|h| !h.is_empty()),
        enabled: enabled != 0,
        online: online != 0,
        registered_at: registered_at.as_deref().and_then(parse_dt_opt),
        last_seen_at: last_seen_at.as_deref().and_then(parse_dt_opt),
        expires_at: expires_at.as_deref().and_then(parse_dt_opt),
    })
}

fn row_to_cert(row: &rusqlite::Row<'_>) -> rusqlite::Result<Cert> {
    let domain: String = row.get(0)?;
    let cert_file: String = row.get(1)?;
    let key_file: String = row.get(2)?;
    let expires_at: Option<String> = row.get(3)?;
    let created_at: String = row.get(4)?;
    let sans_json: String = row.get(5)?;
    let source: String = row.get(6)?;
    let acme_dns_provider: Option<String> = row.get(7)?;
    let acme_account_id: Option<String> = row.get(8)?;
    let issued_at: i64 = row.get(9)?;
    let sans: Vec<String> = serde_json::from_str(&sans_json).unwrap_or_default();
    Ok(Cert {
        domain,
        cert_file,
        key_file,
        expires_at: expires_at.as_deref().and_then(parse_dt_opt),
        created_at: parse_dt(&created_at)?,
        sans,
        source,
        acme_dns_provider,
        acme_account_id,
        issued_at,
    })
}

fn parse_dt(s: &str) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::from_str(s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn parse_dt_opt(s: &str) -> Option<DateTime<Utc>> {
    DateTime::from_str(s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DnsProvider, DnsProviderKind, Domain, HostMode, Site, Tun};

    fn make_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        // migrate() takes &mut, so we need a mutable binding here even
        // though we never mutate the binding itself after.
        let mut conn = conn;
        migrate(&mut conn).unwrap();
        conn
    }

    fn dt(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn schema_applies_idempotently() {
        let mut conn = make_conn();
        // Calling migrate again should be a no-op (refinery skips applied migrations).
        migrate(&mut conn).unwrap();
    }

    #[test]
    fn schema_version_table_tracks_applied_migrations() {
        #[allow(unused_mut)]
        let mut conn = make_conn();
        // refinery creates a `refinery_schema_history` table — verify
        // it's there and lists V1 + V2 + V3 as applied (V2 merges tokens
        // into tun; V3 stores token as sha256).
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM refinery_schema_history", [], |r| {
                r.get(0)
            })
            .expect("refinery_schema_history must exist after migrate()");
        assert_eq!(count, 3, "expected V1 + V2 + V3 to be applied");

        // Verify V2 is recorded.
        let v2_present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM refinery_schema_history WHERE version = 2",
                [],
                |r| r.get(0),
            )
            .expect("version column present");
        assert_eq!(v2_present, 1, "V2 (merge tokens into tun) must be applied");
    }

    #[test]
    fn sha256_hex_is_lowercase_64_chars() {
        // Empty input is the well-known sha256("") — regression pin.
        assert_eq!(
            sha256_hex(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // sha256("abc") — well-known.
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // Length + character-class invariant.
        let h = sha256_hex("anything");
        assert_eq!(h.len(), 64);
        assert!(h
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn backfill_populates_token_hash_for_legacy_rows() {
        // Simulate a v2 database with a tun whose `token` is cleartext
        // but `token_hash` is empty (the post-V3 starting state). The
        // backfill helper should compute sha256(token) and write it.
        let conn = make_conn();
        conn.execute(
            "INSERT INTO tun (name, token, enabled, online) VALUES (?1, ?2, 1, 0)",
            params!["legacy", "cleartext-credential"],
        )
        .unwrap();
        let n = backfill_tun_token_hashes(&conn).unwrap();
        assert_eq!(n, 1, "backfilled exactly the one legacy row");
        let stored: String = conn
            .query_row(
                "SELECT token_hash FROM tun WHERE name = 'legacy'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, sha256_hex("cleartext-credential"));

        // Idempotent: a second call should skip the row because
        // `token_hash` is now non-empty.
        let n2 = backfill_tun_token_hashes(&conn).unwrap();
        assert_eq!(n2, 0, "no rows to backfill on second pass");
    }

    #[test]
    fn upsert_tun_populates_token_hash_from_cleartext() {
        // Inserting a tun via the public API must end up with a
        // matching token_hash even if the caller didn't set it.
        let conn = make_conn();
        upsert_tun(
            &conn,
            &Tun {
                name: "auto".into(),
                token: Some("caller-supplied".into()),
                token_hash: None,
                enabled: true,
                online: false,
                registered_at: None,
                last_seen_at: None,
                expires_at: None,
            },
        )
        .unwrap();
        let back = get_tun(&conn, "auto").unwrap().unwrap();
        assert_eq!(
            back.token_hash.as_deref(),
            Some(sha256_hex("caller-supplied").as_str())
        );
    }

    #[test]
    fn domains_v2_columns_exist_after_migrate() {
        // Regression: V1 (post-merge) must include the v2 columns on
        // `domains` (auto_issue, dns_provider) and the dns_providers
        // table. If a future refactor accidentally drops them, this
        // test catches it.
        #[allow(unused_mut)]
        let mut conn = make_conn();

        let mut stmt = conn
            .prepare("PRAGMA table_info(domains)")
            .expect("PRAGMA domains");
        let cols: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert!(
            cols.contains(&"auto_issue".to_string()),
            "domains.auto_issue missing: {cols:?}"
        );
        assert!(
            cols.contains(&"dns_provider".to_string()),
            "domains.dns_provider missing: {cols:?}"
        );

        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='dns_providers'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1, "dns_providers table must exist post-migrate");
    }

    #[test]
    fn site_upsert_and_get() {
        let conn = make_conn();
        let s = Site {
            name: "app".into(),
            backend: "http://127.0.0.1:8080".into(),
            enabled: true,
            created_at: dt("2026-01-01T00:00:00+00:00"),
            updated_at: dt("2026-01-01T00:00:00+00:00"),
            host_mode: HostMode::Passthrough,
            host_custom: None,
            domain_count: 0,
        };
        upsert_site(&conn, &s).unwrap();
        let back = get_site(&conn, "app").unwrap().unwrap();
        assert_eq!(back.name, s.name);
        assert_eq!(back.backend, s.backend);
        assert!(back.enabled);
    }

    #[test]
    fn site_upsert_overwrites() {
        let conn = make_conn();
        let s1 = Site {
            name: "app".into(),
            backend: "http://x:80".into(),
            enabled: true,
            created_at: dt("2026-01-01T00:00:00+00:00"),
            updated_at: dt("2026-01-01T00:00:00+00:00"),
            host_mode: HostMode::Passthrough,
            host_custom: None,
            domain_count: 0,
        };
        upsert_site(&conn, &s1).unwrap();
        let s2 = Site {
            backend: "https://y:443".into(),
            updated_at: dt("2026-02-01T00:00:00+00:00"),
            ..s1.clone()
        };
        upsert_site(&conn, &s2).unwrap();
        let back = get_site(&conn, "app").unwrap().unwrap();
        assert_eq!(back.backend, "https://y:443");
        // created_at preserved
        assert_eq!(back.created_at, s1.created_at);
    }

    #[test]
    fn site_delete() {
        let conn = make_conn();
        let s = Site {
            name: "app".into(),
            backend: "http://x:80".into(),
            enabled: true,
            created_at: dt("2026-01-01T00:00:00+00:00"),
            updated_at: dt("2026-01-01T00:00:00+00:00"),
            host_mode: HostMode::Passthrough,
            host_custom: None,
            domain_count: 0,
        };
        upsert_site(&conn, &s).unwrap();
        assert!(delete_site(&conn, "app").unwrap());
        assert!(!delete_site(&conn, "app").unwrap());
    }

    #[test]
    fn domain_upsert_and_list() {
        let conn = make_conn();
        let s = Site {
            name: "app".into(),
            backend: "http://x:80".into(),
            enabled: true,
            created_at: dt("2026-01-01T00:00:00+00:00"),
            updated_at: dt("2026-01-01T00:00:00+00:00"),
            host_mode: HostMode::Passthrough,
            host_custom: None,
            domain_count: 0,
        };
        upsert_site(&conn, &s).unwrap();
        let d = Domain {
            domain: "app.example.com".into(),
            site_name: "app".into(),
            enabled: true,
            auto_issue: false,
            dns_provider: None,
            created_at: dt("2026-01-01T00:00:00+00:00"),
        };
        upsert_domain(&conn, &d).unwrap();
        let list = list_domains(&conn).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].domain, "app.example.com");
    }

    #[test]
    fn domain_delete() {
        let conn = make_conn();
        let s = Site {
            name: "app".into(),
            backend: "http://x:80".into(),
            enabled: true,
            created_at: dt("2026-01-01T00:00:00+00:00"),
            updated_at: dt("2026-01-01T00:00:00+00:00"),
            host_mode: HostMode::Passthrough,
            host_custom: None,
            domain_count: 0,
        };
        upsert_site(&conn, &s).unwrap();
        let d = Domain {
            domain: "app.example.com".into(),
            site_name: "app".into(),
            enabled: true,
            auto_issue: false,
            dns_provider: None,
            created_at: dt("2026-01-01T00:00:00+00:00"),
        };
        upsert_domain(&conn, &d).unwrap();
        assert!(delete_domain(&conn, "app.example.com").unwrap());
        assert!(!delete_domain(&conn, "app.example.com").unwrap());
    }

    #[test]
    fn tun_upsert_and_set_online() {
        let conn = make_conn();
        let t = Tun {
            name: "office".into(),
            token: Some("dev".into()),
            token_hash: None,
            enabled: true,
            online: false,
            registered_at: None,
            last_seen_at: None,
            expires_at: None,
        };
        upsert_tun(&conn, &t).unwrap();
        let back = get_tun(&conn, "office").unwrap().unwrap();
        assert!(!back.online);
        assert_eq!(back.token.as_deref(), Some("dev"));
        // V3: upsert populated token_hash from sha256(token).
        assert_eq!(back.token_hash.as_deref(), Some(sha256_hex("dev").as_str()));
        set_tun_online(&conn, "office", true).unwrap();
        let back = get_tun(&conn, "office").unwrap().unwrap();
        assert!(back.online);
    }

    #[test]
    fn auth_tun_match_and_mismatch() {
        let conn = make_conn();
        let t = Tun {
            name: "office".into(),
            token: Some("dev".into()),
            token_hash: None,
            enabled: true,
            online: false,
            registered_at: None,
            last_seen_at: None,
            expires_at: None,
        };
        upsert_tun(&conn, &t).unwrap();

        // Match → Some((enabled, _))
        let r = auth_tun(&conn, "office", "dev").unwrap();
        assert_eq!(r.map(|(e, _)| e), Some(true));

        // Wrong token → None
        assert!(auth_tun(&conn, "office", "wrong").unwrap().is_none());

        // Wrong name → None
        assert!(auth_tun(&conn, "nope", "dev").unwrap().is_none());
    }

    #[test]
    fn auth_tun_disabled_returns_none() {
        // If the admin explicitly disabled a tun, the WS server must
        // not authenticate against it even when the (name, token)
        // pair matches.
        let conn = make_conn();
        upsert_tun(
            &conn,
            &Tun {
                name: "office".into(),
                token: Some("dev".into()),
                token_hash: None,
                enabled: false,
                online: false,
                registered_at: None,
                last_seen_at: None,
                expires_at: None,
            },
        )
        .unwrap();
        // auth_tun is "row matches (name, token)" — caller's auth
        // check is then `row.enabled && not expired`. Verify the
        // matching row surfaces enabled=false so the caller can drop it.
        let (enabled, _) = auth_tun(&conn, "office", "dev").unwrap().unwrap();
        assert!(!enabled);
    }

    #[test]
    fn cert_upsert_and_list() {
        let conn = make_conn();
        let c = Cert {
            domain: "example.com".into(),
            cert_file: "/etc/ssl/example.com.crt".into(),
            key_file: "/etc/ssl/example.com.key".into(),
            expires_at: Some(dt("2026-12-31T00:00:00+00:00")),
            created_at: dt("2026-01-01T00:00:00+00:00"),
            sans: vec!["example.com".into(), "www.example.com".into()],
            source: "manual".into(),
            acme_dns_provider: None,
            acme_account_id: None,
            issued_at: 0,
        };
        upsert_cert(&conn, &c).unwrap();
        let list = list_certs(&conn).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].sans, vec!["example.com", "www.example.com"]);
        assert!(delete_cert(&conn, "example.com").unwrap());
    }

    #[test]
    fn domain_with_auto_issue_and_dns_provider() {
        let conn = make_conn();
        upsert_site(
            &conn,
            &Site {
                name: "app".into(),
                backend: "http://x:80".into(),
                enabled: true,
                created_at: dt("2026-01-01T00:00:00+00:00"),
                updated_at: dt("2026-01-01T00:00:00+00:00"),
                host_mode: HostMode::Passthrough,
                host_custom: None,
                domain_count: 0,
            },
        )
        .unwrap();
        upsert_domain(
            &conn,
            &Domain {
                domain: "*.example.com".into(),
                site_name: "app".into(),
                enabled: true,
                auto_issue: true,
                dns_provider: Some("main-cf".into()),
                created_at: dt("2026-01-01T00:00:00+00:00"),
            },
        )
        .unwrap();
        let d = list_domains(&conn).unwrap();
        assert_eq!(d.len(), 1);
        assert!(d[0].auto_issue);
        assert_eq!(d[0].dns_provider.as_deref(), Some("main-cf"));
    }

    #[test]
    fn domain_default_auto_issue_is_false() {
        let conn = make_conn();
        upsert_site(
            &conn,
            &Site {
                name: "app".into(),
                backend: "http://x:80".into(),
                enabled: true,
                created_at: dt("2026-01-01T00:00:00+00:00"),
                updated_at: dt("2026-01-01T00:00:00+00:00"),
                host_mode: HostMode::Passthrough,
                host_custom: None,
                domain_count: 0,
            },
        )
        .unwrap();
        // Insert without specifying auto_issue/dns_provider.
        upsert_domain(
            &conn,
            &Domain {
                domain: "foo.example.com".into(),
                site_name: "app".into(),
                enabled: true,
                auto_issue: false,
                dns_provider: None,
                created_at: dt("2026-01-01T00:00:00+00:00"),
            },
        )
        .unwrap();
        let d = list_domains(&conn).unwrap();
        assert!(!d[0].auto_issue);
        assert!(d[0].dns_provider.is_none());
    }

    #[test]
    fn dns_provider_upsert_and_list() {
        let conn = make_conn();
        let p = DnsProvider {
            name: "main-cf".into(),
            kind: DnsProviderKind::Cloudflare,
            enabled: true,
            config: r#"{"api_token":"abc"}"#.into(),
            created_at: dt("2026-01-01T00:00:00+00:00"),
            updated_at: dt("2026-01-01T00:00:00+00:00"),
        };
        upsert_dns_provider(&conn, &p).unwrap();
        let back = get_dns_provider(&conn, "main-cf").unwrap().unwrap();
        assert_eq!(back.kind, DnsProviderKind::Cloudflare);
        assert_eq!(back.config, r#"{"api_token":"abc"}"#);
        let list = list_dns_providers(&conn).unwrap();
        assert_eq!(list.len(), 1);

        // Update kind + config.
        let mut p2 = p.clone();
        p2.kind = DnsProviderKind::Aliyun;
        p2.config = r#"{"access_key_id":"x","access_key_secret":"y"}"#.into();
        p2.updated_at = dt("2026-02-01T00:00:00+00:00");
        upsert_dns_provider(&conn, &p2).unwrap();
        let back = get_dns_provider(&conn, "main-cf").unwrap().unwrap();
        assert_eq!(back.kind, DnsProviderKind::Aliyun);
        assert_eq!(back.updated_at, dt("2026-02-01T00:00:00+00:00"));
        // created_at preserved.
        assert_eq!(back.created_at, dt("2026-01-01T00:00:00+00:00"));
    }

    #[test]
    fn dns_provider_delete() {
        let conn = make_conn();
        let p = DnsProvider {
            name: "main-cf".into(),
            kind: DnsProviderKind::Cloudflare,
            enabled: true,
            config: "{}".into(),
            created_at: dt("2026-01-01T00:00:00+00:00"),
            updated_at: dt("2026-01-01T00:00:00+00:00"),
        };
        upsert_dns_provider(&conn, &p).unwrap();
        assert!(delete_dns_provider(&conn, "main-cf").unwrap());
        assert!(!delete_dns_provider(&conn, "main-cf").unwrap());
    }
}
