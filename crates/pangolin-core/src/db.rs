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
use crate::types::{Cert, CertErrorClass, CertStatus, DnsProvider, Domain, Site, Tun};

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
                sans, source, acme_dns_provider, acme_account_id, issued_at,
                status, started_at, last_error,
                next_retry_at, error_class, attempt_count, order_url
         FROM certs ORDER BY domain",
    )?;
    let rows = stmt.query_map([], row_to_cert)?;
    rows.collect()
}

/// List certs whose `status` is one of the given values, ordered by
/// most-recent activity first (`started_at` DESC, NULLs last) so the
/// status-filtered table view surfaces freshly attempted rows at the top.
///
/// Empty `statuses` returns an empty Vec (no rows) — the caller should
/// use [`list_certs`] for the unfiltered case.
pub fn list_certs_by_status(
    conn: &Connection,
    statuses: &[CertStatus],
) -> rusqlite::Result<Vec<Cert>> {
    if statuses.is_empty() {
        return Ok(Vec::new());
    }
    // Build the IN-clause inline: `status IN (?, ?, ?)`. Status values
    // are well-known constants (`CertStatus::as_str`) so this is a
    // safe parameter list, not user input concatenation.
    let placeholders: Vec<&str> = statuses.iter().map(|_| "?").collect();
    let sql = format!(
        "SELECT domain, cert_file, key_file, expires_at, created_at,
                sans, source, acme_dns_provider, acme_account_id, issued_at,
                status, started_at, last_error,
                next_retry_at, error_class, attempt_count, order_url
         FROM certs
         WHERE status IN ({})
         ORDER BY COALESCE(started_at, created_at) DESC, domain",
        placeholders.join(",")
    );
    let mut stmt = conn.prepare(&sql)?;
    let values: Vec<String> = statuses.iter().map(|s| s.as_str().to_string()).collect();
    let params_iter = rusqlite::params_from_iter(values.iter());
    let rows = stmt.query_map(params_iter, row_to_cert)?;
    rows.collect()
}

/// Atomically transition a cert row to a new status, recording the
/// `last_error` (cleared on success) and optionally bumping `started_at`.
///
/// Returns `true` when the row existed and was updated, `false` when the
/// domain has no row in `certs` (caller must `upsert_cert` first — this
/// helper deliberately does not insert because the schema's NOT NULL
/// `cert_file`/`key_file` columns have no sensible default).
///
/// `started_at` semantics: set to `Some(now)` whenever a new ACME attempt
/// begins (Pending → Issuing transition, manual retry). Pass `None` to
/// keep the existing value (e.g. on the Issuing → Issued/Failed transition).
pub fn set_cert_status_atomic(
    conn: &Connection,
    domain: &str,
    status: CertStatus,
    last_error: Option<&str>,
    started_at: Option<DateTime<Utc>>,
) -> rusqlite::Result<bool> {
    // Two SQL shapes: one that touches `started_at`, one that leaves
    // it alone. Using a single statement with a CASE WHEN would be
    // shorter, but the row-touched count would still differ from the
    // semantics callers want (they pass `None` precisely so the
    // existing timestamp is preserved).
    let n = match started_at {
        Some(ts) => conn.execute(
            "UPDATE certs
                SET status = ?1, last_error = ?2, started_at = ?3
              WHERE domain = ?4",
            params![status.as_str(), last_error, ts.to_rfc3339(), domain],
        )?,
        None => conn.execute(
            "UPDATE certs
                SET status = ?1, last_error = ?2
              WHERE domain = ?3",
            params![status.as_str(), last_error, domain],
        )?,
    };
    Ok(n > 0)
}

/// Atomically apply the V5 outcome of a failed ACME attempt: set the
/// new status, the error class (so the UI can render it), the next
/// retry timestamp (so the loop schedules itself), and bump the
/// attempt counter (so backoff escalates). All in one UPDATE so
/// concurrent readers never see a half-applied state.
///
/// `error_class` is `Some(class)` for the failing case, `None` when
/// the row is being cleared (e.g. on transition to `Issued` or
/// `Permanent` where the loop must not retry on its own).
pub fn set_cert_failure(
    conn: &Connection,
    domain: &str,
    status: CertStatus,
    last_error: &str,
    error_class: &CertErrorClass,
    next_retry_at: DateTime<Utc>,
    attempt_count: u32,
) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE certs
            SET status = ?1,
                last_error = ?2,
                error_class = ?3,
                next_retry_at = ?4,
                attempt_count = ?5
          WHERE domain = ?6",
        params![
            status.as_str(),
            Some(last_error),
            Some(error_class.as_str()),
            next_retry_at.to_rfc3339(),
            attempt_count as i64,
            domain,
        ],
    )?;
    Ok(n > 0)
}

/// Clear the V5 failure fields on a successful issuance. The retry
/// counter resets to 0 and `next_retry_at` / `error_class` go to NULL
/// so the renewal loop will treat the row as healthy and the next
/// expiry check will be the one to drive a renewal.
pub fn clear_cert_failure(conn: &Connection, domain: &str) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE certs
            SET last_error = NULL,
                error_class = NULL,
                next_retry_at = NULL,
                attempt_count = 0
          WHERE domain = ?1",
        params![domain],
    )?;
    Ok(n > 0)
}

/// Record the in-flight instant-acme order URL on the row, so the
/// loop can resume polling it after a restart instead of opening a
/// fresh order (and burning another rate-limit slot).
pub fn set_cert_order_url(
    conn: &Connection,
    domain: &str,
    order_url: Option<&str>,
) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE certs
            SET order_url = ?1,
                status = CASE WHEN ?1 IS NULL THEN status ELSE 'issuing' END
          WHERE domain = ?2",
        params![order_url, domain],
    )?;
    Ok(n > 0)
}

/// Find the next `next_retry_at` across all rows in a retryable
/// failure state. Used by `AcmeService::run` to pick the sleep
/// duration for the per-row schedule: instead of a fixed 6h ticker,
/// the loop sleeps until the earliest due row.
///
/// `None` means "no rows are due; the loop can sleep the full
/// `idle_sleep` (default 6h) before re-checking".
pub fn earliest_pending_retry(conn: &Connection) -> rusqlite::Result<Option<DateTime<Utc>>> {
    let mut stmt = conn.prepare(
        "SELECT MIN(next_retry_at)
         FROM certs
         WHERE next_retry_at IS NOT NULL
           AND status IN ('failed', 'rate_limited', 'skipped')",
    )?;
    let raw: Option<String> = stmt.query_row([], |row| row.get(0))?;
    Ok(raw.as_deref().and_then(parse_dt_opt))
}

/// Aggregate count of rows per [`CertStatus`]. Every variant appears in
/// the result (zero-valued when no rows match) so dashboard rendering
/// doesn't have to special-case missing keys.
///
/// Backed by `idx_certs_status` (V4) so this stays O(distinct statuses)
/// rather than O(rows).
pub fn count_certs_by_status(
    conn: &Connection,
) -> rusqlite::Result<std::collections::HashMap<CertStatus, usize>> {
    let mut counts: std::collections::HashMap<CertStatus, usize> =
        CertStatus::all().iter().map(|s| (*s, 0_usize)).collect();
    let mut stmt = conn.prepare("SELECT status, COUNT(*) FROM certs GROUP BY status")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let raw: String = row.get(0)?;
        let n: i64 = row.get(1)?;
        // Unknown statuses (e.g. data from a downgrade) are silently
        // dropped — the UI would render them as a fifth bucket with no
        // meaning anyway.
        if let Ok(s) = raw.parse::<CertStatus>() {
            *counts.entry(s).or_insert(0) = n as usize;
        }
    }
    Ok(counts)
}

/// Look up a single cert row by domain. Cheaper than `list_certs()
/// .find()` when only one row is needed.
pub fn get_cert(conn: &Connection, domain: &str) -> rusqlite::Result<Option<Cert>> {
    let mut stmt = conn.prepare(
        "SELECT domain, cert_file, key_file, expires_at, created_at,
                sans, source, acme_dns_provider, acme_account_id, issued_at,
                status, started_at, last_error,
                next_retry_at, error_class, attempt_count, order_url
         FROM certs WHERE domain = ?1",
    )?;
    stmt.query_row(params![domain], row_to_cert).optional()
}

pub fn upsert_cert(conn: &Connection, cert: &Cert) -> rusqlite::Result<()> {
    let sans_json = serde_json::to_string(&cert.sans).unwrap_or_else(|_| "[]".to_string());
    let error_class_str = cert.error_class.as_ref().map(|c| c.as_str());
    conn.execute(
        "INSERT INTO certs (domain, cert_file, key_file, expires_at, created_at,
                             sans, source, acme_dns_provider, acme_account_id, issued_at,
                             status, started_at, last_error,
                             next_retry_at, error_class, attempt_count, order_url)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
         ON CONFLICT(domain) DO UPDATE SET
            cert_file = excluded.cert_file,
            key_file = excluded.key_file,
            expires_at = excluded.expires_at,
            sans = excluded.sans,
            source = excluded.source,
            acme_dns_provider = excluded.acme_dns_provider,
            acme_account_id = excluded.acme_account_id,
            issued_at = excluded.issued_at,
            status = excluded.status,
            started_at = excluded.started_at,
            last_error = excluded.last_error,
            next_retry_at = excluded.next_retry_at,
            error_class = excluded.error_class,
            attempt_count = excluded.attempt_count,
            order_url = excluded.order_url",
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
            cert.status.as_str(),
            cert.started_at.map(|t| t.to_rfc3339()),
            cert.last_error,
            cert.next_retry_at.map(|t| t.to_rfc3339()),
            error_class_str,
            cert.attempt_count,
            cert.order_url,
        ],
    )?;
    Ok(())
}

pub fn delete_cert(conn: &Connection, domain: &str) -> rusqlite::Result<bool> {
    let n = conn.execute("DELETE FROM certs WHERE domain = ?1", params![domain])?;
    Ok(n > 0)
}

/// Insert a `Pending` placeholder cert row for `domain` if and only if no
/// cert row already exists. Used by `handle_create` immediately after
/// `upsert_domain` (when `auto_issue=true`) and by `AcmeState::ensure_one`
/// at the start of every renewal scan, so the admin UI sees a row from
/// the moment auto-issue is enabled — no vacuum window between domain
/// creation and the first ACME tick.
///
/// Idempotent: if a row exists in any status (Issued, Failed, Skipped,
/// Pending, Issuing) this is a no-op. The lifecycle is then driven by
/// `set_cert_status_atomic` from `ensure_one`. Specifically, toggling
/// `auto_issue=false` does NOT delete the row, and toggling it back on
/// does NOT reset a prior Failed status — operators see the history
/// until they explicitly retry or delete.
///
/// Returns `true` if a row was inserted, `false` if a row already existed.
///
/// `cert_file`/`key_file` are populated with placeholder paths under the
/// configured cert_dir; the real blob path is written by ACME on success
/// via `upsert_cert`. Placeholder paths never reach the TLS handshake
/// because the row's `status` is not `Issued` until then.
pub fn ensure_pending_cert_row(
    conn: &Connection,
    domain: &str,
    cert_dir: &Path,
) -> rusqlite::Result<bool> {
    // Check first — both because we want a precise return value and
    // because `INSERT OR IGNORE` would silently swallow the conflict
    // without telling us whether anything changed.
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM certs WHERE domain = ?1",
            params![domain],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if exists {
        return Ok(false);
    }
    // Placeholder blob path — same convention as `CertManager::resolve_cert`
    // so that, once ACME writes the real blob, the path matches what TLS
    // handshake expects to read.
    let blob_path = cert_dir.join(domain).to_string_lossy().into_owned();
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO certs (domain, cert_file, key_file, created_at,
                            sans, source, issued_at,
                            status, started_at, last_error)
         VALUES (?1, ?2, ?3, ?4, ?5, 'acme', 0, 'pending', ?6, NULL)",
        params![
            domain,
            blob_path,
            blob_path,
            now,
            serde_json::to_string(&[domain]).unwrap_or_else(|_| "[]".into()),
            now,
        ],
    )?;
    Ok(true)
}

/// Stale-`Issuing` watchdog (issue #45 follow-up).
///
/// `ensure_one` transitions a cert row to `Issuing` BEFORE awaiting the
/// blocking ACME call. If the process is killed / panics / OOMs between
/// those two moments, the row is left in `Issuing` forever — the next
/// renewal scan won't ever come back to it because there's no
/// out-of-band timer that says "this took too long, give up". The
/// row sits there showing a blue spinner until an operator either
/// retries manually or restarts and waits `renew_check_interval_hours`.
///
/// This helper, called once at `App::new` startup, demotes every row
/// whose `status = 'issuing'` and `started_at` is older than `threshold`
/// to `Failed` with a `last_error` of "issuance interrupted (process
/// restart or timeout)". Rows whose `started_at` is recent (or NULL,
/// which is the case after a manual DB poke) are left alone — that
/// would race against an actually-in-flight issuance on a fast restart.
///
/// Returns the domains that were swept so the caller can surface them
/// in startup logs.
pub fn recover_stuck_issuing_rows(
    conn: &Connection,
    threshold: chrono::Duration,
) -> rusqlite::Result<Vec<String>> {
    let cutoff = (chrono::Utc::now() - threshold).to_rfc3339();
    // Two-step (SELECT then UPDATE) so we can return the swept domains.
    // RETURNING would be cleaner but rusqlite's `execute` doesn't expose
    // it; a small SELECT + UPDATE matches the rest of the helper module.
    let mut stmt = conn.prepare(
        "SELECT domain FROM certs
         WHERE status = 'issuing' AND started_at IS NOT NULL AND started_at < ?1",
    )?;
    let stuck: Vec<String> = stmt
        .query_map(params![cutoff], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if stuck.is_empty() {
        return Ok(stuck);
    }
    conn.execute(
        "UPDATE certs
            SET status = 'failed',
                last_error = 'issuance interrupted (process restart or timeout)'
          WHERE status = 'issuing' AND started_at IS NOT NULL AND started_at < ?1",
        params![cutoff],
    )?;
    Ok(stuck)
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
    let status_raw: String = row.get(10)?;
    let started_at: Option<String> = row.get(11)?;
    let last_error: Option<String> = row.get(12)?;
    let next_retry_at: Option<String> = row.get(13)?;
    let error_class: Option<String> = row.get(14)?;
    let attempt_count: i64 = row.get(15)?;
    let order_url: Option<String> = row.get(16)?;
    let sans: Vec<String> = serde_json::from_str(&sans_json).unwrap_or_default();
    // Unknown statuses (downgrade artefact, manual SQL edit) fall back to
    // the conservative `Issued` default — the cert is still on disk, so
    // hiding it would be worse than showing it without a fresh badge.
    let status = status_raw
        .parse::<CertStatus>()
        .unwrap_or(CertStatus::Issued);
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
        status,
        started_at: started_at.as_deref().and_then(parse_dt_opt),
        last_error,
        next_retry_at: next_retry_at.as_deref().and_then(parse_dt_opt),
        error_class: error_class.as_deref().and_then(CertErrorClass::parse),
        attempt_count: attempt_count.max(0) as u32,
        order_url,
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
        // it's there and lists V1..V5 as applied. (V2 merges
        // tokens into tun; V3 stores token as sha256; V4 adds the
        // ACME-lifecycle columns on `certs` for issue #45; V5 adds
        // next_retry_at / error_class / attempt_count / order_url for
        // the per-row backoff schedule.)
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM refinery_schema_history", [], |r| {
                r.get(0)
            })
            .expect("refinery_schema_history must exist after migrate()");
        assert_eq!(count, 5, "expected V1 + V2 + V3 + V4 + V5 to be applied");

        // Verify V2 is recorded.
        let v2_present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM refinery_schema_history WHERE version = 2",
                [],
                |r| r.get(0),
            )
            .expect("version column present");
        assert_eq!(v2_present, 1, "V2 (merge tokens into tun) must be applied");

        // Verify V4 is recorded.
        let v4_present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM refinery_schema_history WHERE version = 4",
                [],
                |r| r.get(0),
            )
            .expect("version column present");
        assert_eq!(v4_present, 1, "V4 (cert status lifecycle) must be applied");
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
            status: CertStatus::Issued,
            started_at: None,
            last_error: None,
            next_retry_at: None,
            error_class: None,
            attempt_count: 0,
            order_url: None,
        };
        upsert_cert(&conn, &c).unwrap();
        let list = list_certs(&conn).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].sans, vec!["example.com", "www.example.com"]);
        assert_eq!(list[0].status, CertStatus::Issued);
        assert!(list[0].started_at.is_none());
        assert!(list[0].last_error.is_none());
        assert!(list[0].next_retry_at.is_none());
        assert!(list[0].error_class.is_none());
        assert_eq!(list[0].attempt_count, 0);
        assert!(delete_cert(&conn, "example.com").unwrap());
    }

    // ──────────────────────────────────────────────────────────────────
    // V4 — cert status lifecycle helpers (issue #45).
    // ──────────────────────────────────────────────────────────────────

    fn make_cert(domain: &str, status: CertStatus, last_error: Option<&str>) -> Cert {
        Cert {
            domain: domain.into(),
            cert_file: format!("/blob/{}", domain),
            key_file: format!("/blob/{}", domain),
            expires_at: None,
            created_at: dt("2026-01-01T00:00:00+00:00"),
            sans: vec![domain.into()],
            source: "acme".into(),
            acme_dns_provider: None,
            acme_account_id: None,
            issued_at: 0,
            status,
            started_at: None,
            last_error: last_error.map(String::from),
            next_retry_at: None,
            error_class: None,
            attempt_count: 0,
            order_url: None,
        }
    }

    #[test]
    fn cert_status_defaults_to_issued_for_legacy_rows() {
        // A row inserted before V4 carried no `status` column. V4's
        // `DEFAULT 'issued'` should leave that row valid and visible.
        let conn = make_conn();
        conn.execute(
            "INSERT INTO certs (domain, cert_file, key_file, created_at, sans, source, issued_at)
             VALUES (?1, ?2, ?3, ?4, '[]', 'manual', 0)",
            params![
                "legacy.example.com",
                "/blob/legacy.example.com",
                "/blob/legacy.example.com",
                "2026-01-01T00:00:00+00:00",
            ],
        )
        .unwrap();
        let list = list_certs(&conn).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].status, CertStatus::Issued);
        assert!(list[0].last_error.is_none());
    }

    #[test]
    fn set_cert_status_atomic_updates_existing_row() {
        let conn = make_conn();
        upsert_cert(
            &conn,
            &make_cert("a.example.com", CertStatus::Pending, None),
        )
        .unwrap();
        let updated = set_cert_status_atomic(
            &conn,
            "a.example.com",
            CertStatus::Issuing,
            None,
            Some(dt("2026-06-01T00:00:00+00:00")),
        )
        .unwrap();
        assert!(updated);
        let c = get_cert(&conn, "a.example.com").unwrap().unwrap();
        assert_eq!(c.status, CertStatus::Issuing);
        assert_eq!(c.started_at, Some(dt("2026-06-01T00:00:00+00:00")));
        assert!(c.last_error.is_none());

        // Transition to Failed — last_error captured, started_at preserved
        // (None means "don't touch").
        let updated = set_cert_status_atomic(
            &conn,
            "a.example.com",
            CertStatus::Failed,
            Some("boom"),
            None,
        )
        .unwrap();
        assert!(updated);
        let c = get_cert(&conn, "a.example.com").unwrap().unwrap();
        assert_eq!(c.status, CertStatus::Failed);
        assert_eq!(c.last_error.as_deref(), Some("boom"));
        assert_eq!(c.started_at, Some(dt("2026-06-01T00:00:00+00:00")));

        // Transition to Issued clears last_error.
        let updated =
            set_cert_status_atomic(&conn, "a.example.com", CertStatus::Issued, None, None).unwrap();
        assert!(updated);
        let c = get_cert(&conn, "a.example.com").unwrap().unwrap();
        assert_eq!(c.status, CertStatus::Issued);
        assert!(c.last_error.is_none());
    }

    #[test]
    fn set_cert_status_atomic_returns_false_for_missing_row() {
        let conn = make_conn();
        let updated =
            set_cert_status_atomic(&conn, "ghost.example.com", CertStatus::Failed, None, None)
                .unwrap();
        assert!(!updated);
    }

    #[test]
    fn list_certs_by_status_returns_only_matching_rows() {
        let conn = make_conn();
        upsert_cert(
            &conn,
            &make_cert("a.example.com", CertStatus::Pending, None),
        )
        .unwrap();
        upsert_cert(
            &conn,
            &make_cert("b.example.com", CertStatus::Failed, Some("err")),
        )
        .unwrap();
        upsert_cert(&conn, &make_cert("c.example.com", CertStatus::Issued, None)).unwrap();

        let only_failed = list_certs_by_status(&conn, &[CertStatus::Failed]).unwrap();
        assert_eq!(only_failed.len(), 1);
        assert_eq!(only_failed[0].domain, "b.example.com");

        let in_flight =
            list_certs_by_status(&conn, &[CertStatus::Pending, CertStatus::Issuing]).unwrap();
        assert_eq!(in_flight.len(), 1);
        assert_eq!(in_flight[0].domain, "a.example.com");

        // Empty status list → empty result (caller's responsibility to
        // use list_certs() for the unfiltered case).
        let empty = list_certs_by_status(&conn, &[]).unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn count_certs_by_status_includes_zero_buckets() {
        let conn = make_conn();
        upsert_cert(
            &conn,
            &make_cert("a.example.com", CertStatus::Pending, None),
        )
        .unwrap();
        upsert_cert(
            &conn,
            &make_cert("b.example.com", CertStatus::Failed, Some("err")),
        )
        .unwrap();
        upsert_cert(&conn, &make_cert("c.example.com", CertStatus::Issued, None)).unwrap();
        upsert_cert(&conn, &make_cert("d.example.com", CertStatus::Issued, None)).unwrap();

        let counts = count_certs_by_status(&conn).unwrap();
        // Every variant present, including the zero-valued ones.
        // (V5 added `RateLimited` and `Permanent` to the CertStatus
        // enum, so the count goes from 5 to 7.)
        assert_eq!(counts.len(), 7);
        assert_eq!(counts[&CertStatus::Pending], 1);
        assert_eq!(counts[&CertStatus::Issuing], 0);
        assert_eq!(counts[&CertStatus::Issued], 2);
        assert_eq!(counts[&CertStatus::Failed], 1);
        assert_eq!(counts[&CertStatus::Skipped], 0);
        assert_eq!(counts[&CertStatus::RateLimited], 0);
        assert_eq!(counts[&CertStatus::Permanent], 0);
    }

    #[test]
    fn ensure_pending_cert_row_inserts_when_missing() {
        let conn = make_conn();
        let dir = std::path::Path::new("/tmp/certs");
        let inserted = ensure_pending_cert_row(&conn, "new.example.com", dir).unwrap();
        assert!(inserted);
        let c = get_cert(&conn, "new.example.com").unwrap().unwrap();
        assert_eq!(c.status, CertStatus::Pending);
        assert!(c.started_at.is_some());
        assert!(c.last_error.is_none());
        assert_eq!(c.source, "acme");
        // Placeholder blob path follows CertManager::resolve_cert's
        // host-named convention so the row's path matches the eventual
        // ACME-written blob.
        assert!(c.cert_file.ends_with("/new.example.com"));
        assert_eq!(c.cert_file, c.key_file);
    }

    #[test]
    fn ensure_pending_cert_row_preserves_existing_history() {
        let conn = make_conn();
        let dir = std::path::Path::new("/tmp/certs");
        // Pre-existing Failed row: must NOT be overwritten back to
        // Pending — the operator needs to see the failure history.
        upsert_cert(
            &conn,
            &make_cert("old.example.com", CertStatus::Failed, Some("boom")),
        )
        .unwrap();
        let inserted = ensure_pending_cert_row(&conn, "old.example.com", dir).unwrap();
        assert!(!inserted, "second call must be a no-op");
        let c = get_cert(&conn, "old.example.com").unwrap().unwrap();
        assert_eq!(c.status, CertStatus::Failed);
        assert_eq!(c.last_error.as_deref(), Some("boom"));
    }

    #[test]
    fn recover_stuck_issuing_demotes_old_rows_only() {
        // The startup watchdog must sweep rows whose Issuing transition
        // happened long ago (process restart / OOM / panic), but leave
        // recent rows alone — a fresh restart on a still-running ACME
        // call would otherwise stomp on an in-flight issuance.
        let conn = make_conn();
        // Stale row: started_at well past the 10-minute window.
        let mut stale = make_cert("stale.example.com", CertStatus::Issuing, None);
        stale.started_at = Some(dt("2026-01-01T00:00:00+00:00"));
        upsert_cert(&conn, &stale).unwrap();
        // Recent row: started_at "now" — should NOT be touched.
        let mut recent = make_cert("recent.example.com", CertStatus::Issuing, None);
        recent.started_at = Some(chrono::Utc::now());
        upsert_cert(&conn, &recent).unwrap();
        // Unrelated row in a different status — never touched regardless.
        upsert_cert(
            &conn,
            &make_cert("ok.example.com", CertStatus::Issued, None),
        )
        .unwrap();

        let swept = recover_stuck_issuing_rows(&conn, chrono::Duration::minutes(10)).unwrap();
        assert_eq!(swept, vec!["stale.example.com".to_string()]);

        let stale_after = get_cert(&conn, "stale.example.com").unwrap().unwrap();
        assert_eq!(stale_after.status, CertStatus::Failed);
        assert_eq!(
            stale_after.last_error.as_deref(),
            Some("issuance interrupted (process restart or timeout)")
        );
        let recent_after = get_cert(&conn, "recent.example.com").unwrap().unwrap();
        assert_eq!(recent_after.status, CertStatus::Issuing);
        let ok_after = get_cert(&conn, "ok.example.com").unwrap().unwrap();
        assert_eq!(ok_after.status, CertStatus::Issued);
    }

    #[test]
    fn recover_stuck_issuing_skips_null_started_at() {
        // Rows poked directly into the DB without `started_at` (e.g. a
        // manual SQL repair) must NOT be swept — we can't tell whether
        // they're old or new. Operator can still hit Retry.
        let conn = make_conn();
        let mut row = make_cert("nullstart.example.com", CertStatus::Issuing, None);
        row.started_at = None;
        upsert_cert(&conn, &row).unwrap();
        let swept = recover_stuck_issuing_rows(&conn, chrono::Duration::seconds(0)).unwrap();
        assert!(swept.is_empty(), "NULL started_at must be left alone");
        let after = get_cert(&conn, "nullstart.example.com").unwrap().unwrap();
        assert_eq!(after.status, CertStatus::Issuing);
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
