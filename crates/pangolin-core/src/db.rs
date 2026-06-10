//! SQLite schema and load functions.
//!
//! Six tables, all with TEXT primary keys (natural keys, no surrogate ids):
//!   sites         (name PK, backend, enabled, ...)
//!   domains       (domain PK, site_name, enabled, auto_issue, dns_provider, created_at) FK→sites.name
//!   tun           (name PK, enabled, online, registered_at, last_seen_at)
//!   tokens        (token PK, enabled, created_at, expires_at)
//!   certs         (domain PK, cert_file, key_file, expires_at, created_at, sans, source, ...)
//!   dns_providers (name PK, kind, enabled, config, created_at, updated_at)
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

use crate::types::{Cert, DnsProvider, Domain, Site, Token, Tun};

/// All five CREATE TABLE statements, idempotent.
pub const SCHEMA_SQL: &str = include_str!("schema.sql");

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

/// Apply the schema. Safe to call on every startup.
pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SCHEMA_SQL)
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

// ---- Tun CRUD ----

pub fn list_tuns(conn: &Connection) -> rusqlite::Result<Vec<Tun>> {
    let mut stmt = conn.prepare(
        "SELECT name, enabled, online, registered_at, last_seen_at
         FROM tun ORDER BY name",
    )?;
    let rows = stmt.query_map([], row_to_tun)?;
    rows.collect()
}

pub fn get_tun(conn: &Connection, name: &str) -> rusqlite::Result<Option<Tun>> {
    let mut stmt = conn.prepare(
        "SELECT name, enabled, online, registered_at, last_seen_at
         FROM tun WHERE name = ?1",
    )?;
    stmt.query_row(params![name], row_to_tun).optional()
}

pub fn upsert_tun(conn: &Connection, tun: &Tun) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO tun (name, enabled, online, registered_at, last_seen_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(name) DO UPDATE SET
            enabled = excluded.enabled,
            online = excluded.online,
            last_seen_at = excluded.last_seen_at",
        params![
            tun.name,
            tun.enabled as i32,
            tun.online as i32,
            tun.registered_at.map(|t| t.to_rfc3339()),
            tun.last_seen_at.map(|t| t.to_rfc3339()),
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

// ---- Token CRUD ----

pub fn list_tokens(conn: &Connection) -> rusqlite::Result<Vec<Token>> {
    let mut stmt = conn.prepare(
        "SELECT token, enabled, created_at, expires_at FROM tokens ORDER BY created_at DESC",
    )?;
    let rows = stmt.query_map([], row_to_token)?;
    rows.collect()
}

pub fn upsert_token(conn: &Connection, token: &Token) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO tokens (token, enabled, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(token) DO UPDATE SET
            enabled = excluded.enabled,
            expires_at = excluded.expires_at",
        params![
            token.token,
            token.enabled as i32,
            token.created_at.to_rfc3339(),
            token.expires_at.map(|t| t.to_rfc3339()),
        ],
    )?;
    Ok(())
}

pub fn delete_token(conn: &Connection, token: &str) -> rusqlite::Result<bool> {
    let n = conn.execute("DELETE FROM tokens WHERE token = ?1", params![token])?;
    Ok(n > 0)
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
    let enabled: i32 = row.get(1)?;
    let online: i32 = row.get(2)?;
    let registered_at: Option<String> = row.get(3)?;
    let last_seen_at: Option<String> = row.get(4)?;
    Ok(Tun {
        name,
        enabled: enabled != 0,
        online: online != 0,
        registered_at: registered_at.as_deref().and_then(parse_dt_opt),
        last_seen_at: last_seen_at.as_deref().and_then(parse_dt_opt),
    })
}

fn row_to_token(row: &rusqlite::Row<'_>) -> rusqlite::Result<Token> {
    let token: String = row.get(0)?;
    let enabled: i32 = row.get(1)?;
    let created_at: String = row.get(2)?;
    let expires_at: Option<String> = row.get(3)?;
    Ok(Token {
        token,
        enabled: enabled != 0,
        created_at: parse_dt(&created_at)?,
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
    use crate::types::{DnsProvider, DnsProviderKind, Domain, HostMode, Site, Token, Tun};

    fn make_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        migrate(&conn).unwrap();
        conn
    }

    fn dt(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn schema_applies_idempotently() {
        let conn = make_conn();
        // Calling migrate again should be a no-op (CREATE IF NOT EXISTS).
        migrate(&conn).unwrap();
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
            enabled: true,
            online: false,
            registered_at: None,
            last_seen_at: None,
        };
        upsert_tun(&conn, &t).unwrap();
        let back = get_tun(&conn, "office").unwrap().unwrap();
        assert!(!back.online);
        set_tun_online(&conn, "office", true).unwrap();
        let back = get_tun(&conn, "office").unwrap().unwrap();
        assert!(back.online);
    }

    #[test]
    fn token_upsert_and_list() {
        let conn = make_conn();
        let t = Token {
            token: "abc123".into(),
            enabled: true,
            created_at: dt("2026-01-01T00:00:00+00:00"),
            expires_at: None,
        };
        upsert_token(&conn, &t).unwrap();
        let list = list_tokens(&conn).unwrap();
        assert_eq!(list.len(), 1);
        assert!(delete_token(&conn, "abc123").unwrap());
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
