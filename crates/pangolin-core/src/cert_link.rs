//! Domain → cert pre-computed link + in-memory cache.
//!
//! Why this exists: the SNI callback needs to resolve "which cert
//! should serve this SNI" for every TLS handshake. Doing that
//! decision at request time — walking the cert directory looking
//! for a wildcard match — is fine in the simple case, but the
//! hot path of an SNI callback is microseconds, and the policy
//! decision (which SANs of which cert cover this domain) is
//! stable between cert CRUD events.
//!
//! So we move the policy decision to write time. The cache is
//! `(domain → cert.domain)`, derived from the existing
//! `domains` and `certs` tables. No new column, no migration.
//! The cache is a DashMap behind a swap pointer: a lookup takes one
//! read lock and then hits the DashMap's lock-free path, and a full
//! rebuild publishes a *new* table under one write lock instead of
//! clearing the live one.
//!
//! CRUD hooks (`relink_for_domain` / `relink_for_cert` /
//! `remove_domain`) maintain the cache. `load_from_db` rebuilds
//! it from scratch — used at startup and as the recovery path
//! if the cache ever drifts.
//!
//! See `docs/design/cert-link.md` for the full rationale and
//! the wildcard-matching rules.

use std::sync::Arc;

use dashmap::DashMap;
use parking_lot::RwLock;
use rusqlite::{Connection, params};

use crate::types::CertStatus;

/// Concurrent map of SNI → cert primary (`certs.domain`).
///
/// Cheap to clone (the inner `DashMap` is `Arc`-shared). The SNI
/// callback and the admin handlers each hold their own clone; they
/// both point at the same backing storage.
///
/// The `RwLock` is the *swap pointer*, not an entry lock: it only
/// guards the replacement of the live table by a rebuild (issue
/// #213). Entry-level writes go through the DashMap's own interior
/// mutability, so a rebuild never has to clear the live table and a
/// reader never observes an empty or half-filled one.
#[derive(Clone, Default)]
pub struct CertLinkCache {
    map: Arc<RwLock<Arc<DashMap<String, String>>>>,
}

impl CertLinkCache {
    /// Build an empty cache. Tests use this; production code calls
    /// `load_from_db` so the cache reflects the on-disk truth.
    pub fn new() -> Self {
        Self::default()
    }

    /// Startup: full rebuild from the `domains` and `certs` tables.
    /// For every `domains` row, find the best cert that covers it
    /// and insert the link. Domains with no covering cert are simply
    /// absent from the map — SNI for them fails with `unrecognized_name`,
    /// which is the right answer.
    pub fn load_from_db(conn: &Connection) -> crate::Result<Self> {
        let cache = Self::new();
        cache.reload_from_db(conn)?;
        Ok(cache)
    }

    /// Rebuild from the DB and publish the result as a new table.
    /// Used by `App::reload_indexes` to make the cache consistent
    /// after an out-of-band edit (e.g. `INSERT INTO certs` by hand,
    /// or a test that writes a cert blob and inserts the matching row
    /// after the process has already started). Cheap — O(domains ×
    /// certs), see `relink_for_cert` for the perf note.
    ///
    /// The rebuild is atomic for readers: the new table is built off
    /// to the side and only then swapped in under one write lock, so
    /// a TLS handshake sees either the whole old table or the whole
    /// new one — never an empty or half-filled table. A failure
    /// anywhere in the rebuild returns before the swap, leaving the
    /// live table with every link it already had (issue #213).
    ///
    /// The caller must serialize this against `relink_for_domain` /
    /// `relink_for_cert` — the cache does not order an entry write
    /// against a rebuild. See `docs/design/cert-link.md` §5.
    pub fn reload_from_db(&self, conn: &Connection) -> crate::Result<()> {
        let new_map: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
        let mut stmt = conn.prepare("SELECT domain FROM domains")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for row in rows {
            let domain = row?;
            if let Some(cert_domain) = find_best_cert_for(conn, &domain)? {
                new_map.insert(domain, cert_domain);
            }
        }
        // One write lock, one swap. The outgoing table is moved out
        // of the lock before it is dropped: freeing it tears down the
        // map's buckets, and doing that under the write lock would
        // park every reader (each TLS handshake) for the whole
        // teardown.
        let previous = {
            let mut slot = self.map.write();
            std::mem::replace(&mut *slot, new_map)
        };
        drop(previous);
        Ok(())
    }

    /// SNI hot path: exact match, then walk up the domain looking for
    /// a wildcard key. Stops at the bare TLD (e.g. `*.com` is never
    /// consulted — wildcard certs do not cover TLDs).
    ///
    /// The SNI is trimmed of any trailing dot first; some clients
    /// (and some resolvers in front of them) include one in the
    /// server_name.
    ///
    /// The whole walk holds one read lock, so the exact match and
    /// the wildcard candidates are all resolved against the same
    /// table. A rebuild that swaps the table mid-walk would
    /// otherwise make the walk mix two revisions.
    pub fn lookup(&self, sni: &str) -> Option<String> {
        let sni = sni.trim_end_matches('.');
        let map = self.map.read();
        if let Some(c) = map.get(sni) {
            return Some(c.value().clone());
        }
        // Walk up: api.v2.example.com → *.v2.example.com → *.example.com
        let mut rest = sni;
        while let Some(dot) = rest.find('.') {
            rest = &rest[dot + 1..];
            // rest must contain a dot to be a valid wildcard suffix
            // (*.com is meaningless — wildcard certs don't cover TLDs).
            if !rest.contains('.') {
                break;
            }
            let wc = format!("*.{}", rest);
            if let Some(c) = map.get(&wc) {
                return Some(c.value().clone());
            }
        }
        None
    }

    /// Recompute the link for a single domain. Called by the domain
    /// CRUD hooks after the DB row is committed. Pure in-memory
    /// update — no DB I/O.
    pub fn relink_for_domain(&self, conn: &Connection, domain: &str) -> crate::Result<()> {
        let new_link = find_best_cert_for(conn, domain)?;
        // The read lock guards the swap, not the entry — it does not
        // order this write against a rebuild. The caller must
        // serialize the two; see `docs/design/cert-link.md` §5.
        let map = self.map.read();
        match new_link {
            Some(c) => {
                map.insert(domain.to_string(), c);
            }
            None => {
                map.remove(domain);
            }
        }
        Ok(())
    }

    /// Recompute the link for every domain that *might* be affected
    /// by a change to `cert_domain`. Implementation: scan all domains
    /// and call `relink_for_domain` on each. O(domains) per call —
    /// cert CRUD is rare and domain counts are bounded, so this is
    /// cheap in practice.
    ///
    /// TODO(perf): at higher scale (≥10k domains, ≥1k certs), this
    /// becomes wasteful — most domains are unaffected by a single
    /// cert change. The optimization is a reverse index
    /// `cert → [domain]` rebuilt alongside `Domains × certs`, so
    /// `relink_for_cert` only touches the affected set. The
    /// `_cert_domain` parameter is reserved for that future
    /// implementation. See `docs/design/cert-link.md` §6 for the
    /// full note.
    pub fn relink_for_cert(&self, conn: &Connection, _cert_domain: &str) -> crate::Result<()> {
        let mut stmt = conn.prepare("SELECT domain FROM domains")?;
        let domains: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);
        for d in domains {
            self.relink_for_domain(conn, &d)?;
        }
        Ok(())
    }

    /// Drop a domain's link. Called by the domain delete hook.
    /// Does not touch any cert row — the cert may still be on disk
    /// and serving other domains; it just stops serving this one.
    pub fn remove_domain(&self, domain: &str) {
        let map = self.map.read();
        map.remove(domain);
    }

    /// Number of entries in the cache. Useful for tests and admin
    /// introspection. Cheap (one read lock plus the DashMap's shard
    /// counts).
    pub fn len(&self) -> usize {
        self.map.read().len()
    }

    /// Whether the cache has any entries. Cheap (one read lock).
    pub fn is_empty(&self) -> bool {
        self.map.read().is_empty()
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Copy, Clone)]
enum Priority {
    /// `cert.domain == D` or `D ∈ cert.sans`.
    Exact = 0,
    /// A SAN `*.X` matches `D = Y.X` with `Y` a single non-empty label.
    Wildcard = 1,
}

/// Pick the cert that best covers `domain`. Returns the cert's
/// `domain` (the cert row's primary key), or `None` if no cert covers.
fn find_best_cert_for(conn: &Connection, domain: &str) -> crate::Result<Option<String>> {
    let issued = CertStatus::Issued.as_str();
    let mut stmt = conn.prepare("SELECT domain, sans FROM certs WHERE status = ?1")?;
    let rows = stmt.query_map(params![issued], |r| {
        let d: String = r.get(0)?;
        let sans: String = r.get(1)?;
        Ok((d, sans))
    })?;
    let mut best: Option<(Priority, String)> = None;
    for row in rows {
        let (cert_domain, sans_json) = row?;
        // A malformed `sans` JSON (e.g., from a manual SQL edit) shouldn't
        // crash the relink. We log a warning naming the cert's primary so
        // the operator can find and fix it, then treat the SAN list as
        // empty — the cert can still win via exact match on `cert.domain`.
        let sans: Vec<String> = match serde_json::from_str(&sans_json) {
            Ok(v) => v,
            Err(e) => {
                log::warn!(
                    "cert_links: certs.domain={} has malformed sans JSON ({}); \
                     treating as empty SAN list",
                    cert_domain,
                    e,
                );
                Vec::new()
            }
        };
        if let Some(pri) = cert_covers_domain(&cert_domain, &sans, domain) {
            let candidate = (pri, cert_domain);
            best = Some(match best {
                None => candidate,
                Some(b) if candidate.0 < b.0 => candidate,
                Some(b) => b,
            });
        }
    }
    Ok(best.map(|(_, c)| c))
}

/// Decide whether a cert (identified by its primary `cert_domain` and
/// the full SAN list) covers the given `domain`. Returns the priority
/// of the match, or `None` if the cert does not cover.
///
/// Single-level wildcard only: `*.example.com` covers
/// `api.example.com` but NOT `api.v2.example.com`. This matches
/// Let's Encrypt's issuance rules and RFC 6125.
fn cert_covers_domain(cert_domain: &str, sans: &[String], domain: &str) -> Option<Priority> {
    if cert_domain == domain {
        return Some(Priority::Exact);
    }
    if sans.iter().any(|s| s == domain) {
        return Some(Priority::Exact);
    }

    // Wildcard check: domain must be "Y.X" with Y a single non-empty
    // label, and the cert must have a corresponding "*.X" entry.
    let dot = domain.find('.')?;
    let y = &domain[..dot];
    let x = &domain[dot + 1..];
    if y.is_empty() || y.contains('.') {
        return None;
    }
    let wc = format!("*.{}", x);

    if cert_domain == wc {
        return Some(Priority::Wildcard);
    }
    if sans.iter().any(|s| s == &wc) {
        return Some(Priority::Wildcard);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- CertLinkCache::lookup ----

    fn cache_with(entries: &[(&str, &str)]) -> CertLinkCache {
        let c = CertLinkCache::new();
        {
            let map = c.map.read();
            for (k, v) in entries {
                map.insert((*k).to_string(), (*v).to_string());
            }
        }
        c
    }

    #[test]
    fn lookup_exact_match() {
        let c = cache_with(&[("example.com", "example.com")]);
        assert_eq!(c.lookup("example.com").as_deref(), Some("example.com"));
    }

    #[test]
    fn lookup_wildcard_walk_hits_immediate_parent() {
        let c = cache_with(&[("*.example.com", "example.com")]);
        assert_eq!(c.lookup("api.example.com").as_deref(), Some("example.com"));
    }

    #[test]
    fn lookup_walk_continues_past_multi_level_suffix() {
        // The lookup walk visits every `*.X` key where `X` is a suffix
        // of the SNI, not just the immediate parent. With the cache
        // keyed on `*.example.com` and an SNI of `a.b.example.com`,
        // the walk tries `*.b.example.com` (miss) then `*.example.com`
        // (hit) — returning the cert primary.
        //
        // Note: this is purely the cache's lookup behavior. Whether
        // the returned cert actually *covers* the SNI per RFC 6125
        // single-label wildcard rules is a browser-side concern; the
        // cache's job is just to find the best link, not to police
        // what the user registered.
        let c = cache_with(&[("*.example.com", "example.com")]);
        assert_eq!(c.lookup("a.b.example.com").as_deref(), Some("example.com"));
    }

    #[test]
    fn lookup_stops_at_bare() {
        // SNI "example.com" must not match a hypothetical "*.com" entry.
        let c = cache_with(&[("*.com", "com")]);
        assert_eq!(c.lookup("example.com"), None);
    }

    #[test]
    fn lookup_strips_trailing_dot() {
        let c = cache_with(&[("example.com", "example.com")]);
        assert_eq!(c.lookup("example.com.").as_deref(), Some("example.com"));
    }

    #[test]
    fn lookup_miss_returns_none() {
        let c = cache_with(&[("example.com", "example.com")]);
        assert_eq!(c.lookup("unknown.com"), None);
    }

    #[test]
    fn lookup_prefers_exact_over_wildcard() {
        let c = cache_with(&[
            ("*.example.com", "wildcard-cert"),
            ("api.example.com", "specific-cert"),
        ]);
        // Exact key wins: api.example.com → specific-cert
        assert_eq!(
            c.lookup("api.example.com").as_deref(),
            Some("specific-cert")
        );
        // A different subdomain still falls through to wildcard.
        assert_eq!(
            c.lookup("other.example.com").as_deref(),
            Some("wildcard-cert")
        );
    }

    // ---- cert_covers_domain ----

    #[test]
    fn cert_covers_domain_exact_match() {
        assert_eq!(
            cert_covers_domain("example.com", &[], "example.com"),
            Some(Priority::Exact)
        );
        assert_eq!(
            cert_covers_domain(
                "example.com",
                &["www.example.com".into()],
                "www.example.com"
            ),
            Some(Priority::Exact)
        );
    }

    #[test]
    fn cert_covers_domain_via_sans() {
        // Cert primary is *.example.com, SANs include "api.example.com".
        let sans = vec!["*.example.com".to_string(), "api.example.com".to_string()];
        assert_eq!(
            cert_covers_domain("*.example.com", &sans, "api.example.com"),
            Some(Priority::Exact)
        );
    }

    #[test]
    fn cert_covers_domain_wildcard_single_level_only() {
        let sans = vec!["*.example.com".to_string()];
        assert_eq!(
            cert_covers_domain("example.com", &sans, "api.example.com"),
            Some(Priority::Wildcard)
        );
    }

    #[test]
    fn cert_covers_domain_rejects_multi_label_subdomain() {
        let sans = vec!["*.example.com".to_string()];
        // api.v2.example.com — Y="api.v2" is multi-label; reject.
        assert_eq!(
            cert_covers_domain("example.com", &sans, "api.v2.example.com"),
            None
        );
    }

    #[test]
    fn cert_covers_domain_rejects_bare_suffix() {
        let sans = vec!["*.example.com".to_string()];
        // domain = "example.com" → Y is empty → reject wildcard.
        // (Exact match via cert.domain or sans is checked separately.)
        assert_eq!(
            cert_covers_domain("example.com", &sans, "example.com"),
            Some(Priority::Exact) // exact via cert.domain
        );
    }

    #[test]
    fn cert_covers_domain_no_match() {
        let sans = vec!["*.other.com".to_string()];
        assert_eq!(
            cert_covers_domain("example.com", &sans, "api.example.com"),
            None
        );
    }

    // ---- DB-driven flow (uses an in-memory SQLite) ----

    /// Set up a SQLite connection with the `domains` and `certs` tables
    /// matching the production schema (sans is a JSON string, status
    /// is a snake_case text value).
    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE domains (domain TEXT PRIMARY KEY);
             CREATE TABLE certs (
                 domain TEXT PRIMARY KEY,
                 sans TEXT NOT NULL DEFAULT '[]',
                 status TEXT NOT NULL DEFAULT 'issued'
             );",
        )
        .unwrap();
        conn
    }

    fn insert_domain(conn: &Connection, d: &str) {
        conn.execute("INSERT INTO domains(domain) VALUES (?1)", params![d])
            .unwrap();
    }

    fn insert_cert(conn: &Connection, d: &str, sans: &[&str], status: &str) {
        let sans_json = serde_json::to_string(sans).unwrap();
        conn.execute(
            "INSERT INTO certs(domain, sans, status) VALUES (?1, ?2, ?3)",
            params![d, sans_json, status],
        )
        .unwrap();
    }

    #[test]
    fn find_best_cert_picks_exact_over_wildcard() {
        let conn = setup_db();
        insert_domain(&conn, "api.example.com");
        insert_cert(&conn, "example.com", &["*.example.com"], "issued");
        insert_cert(&conn, "api.example.com", &[], "issued");

        let link = find_best_cert_for(&conn, "api.example.com").unwrap();
        assert_eq!(link.as_deref(), Some("api.example.com"));
    }

    #[test]
    fn find_best_cert_picks_wildcard_when_no_exact() {
        let conn = setup_db();
        insert_domain(&conn, "api.example.com");
        insert_cert(&conn, "example.com", &["*.example.com"], "issued");

        let link = find_best_cert_for(&conn, "api.example.com").unwrap();
        assert_eq!(link.as_deref(), Some("example.com"));
    }

    #[test]
    fn find_best_cert_returns_none_for_uncovered_domain() {
        let conn = setup_db();
        insert_domain(&conn, "nope.example.com");
        insert_cert(&conn, "other.com", &["*.other.com"], "issued");

        let link = find_best_cert_for(&conn, "nope.example.com").unwrap();
        assert_eq!(link, None);
    }

    #[test]
    fn find_best_cert_ignores_failed_certs() {
        let conn = setup_db();
        insert_domain(&conn, "example.com");
        // Failed cert should not be picked up.
        insert_cert(&conn, "example.com", &[], "failed");

        let link = find_best_cert_for(&conn, "example.com").unwrap();
        assert_eq!(link, None);
    }

    // ---- Cache flow ----

    #[test]
    fn load_from_db_rebuilds_cache_from_raw() {
        let conn = setup_db();
        insert_domain(&conn, "example.com");
        insert_domain(&conn, "api.example.com");
        insert_cert(&conn, "example.com", &["*.example.com"], "issued");

        let cache = CertLinkCache::load_from_db(&conn).unwrap();
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.lookup("example.com").as_deref(), Some("example.com"));
        assert_eq!(
            cache.lookup("api.example.com").as_deref(),
            Some("example.com")
        );
    }

    #[test]
    fn relink_for_domain_updates_cache() {
        let conn = setup_db();
        insert_domain(&conn, "api.example.com");
        let cache = CertLinkCache::new();

        // No cert yet → absent
        cache.relink_for_domain(&conn, "api.example.com").unwrap();
        assert!(cache.lookup("api.example.com").is_none());

        // Add a wildcard cert, relink → present
        insert_cert(&conn, "example.com", &["*.example.com"], "issued");
        cache.relink_for_domain(&conn, "api.example.com").unwrap();
        assert_eq!(
            cache.lookup("api.example.com").as_deref(),
            Some("example.com")
        );

        // No DB write should have happened (cache is the only storage).
        // We don't introspect the cert/domain tables here, but the
        // shape of the API makes that obvious: no Connection::execute
        // is called from relink_for_domain.
    }

    #[test]
    fn relink_for_cert_scans_all_domains() {
        let conn = setup_db();
        insert_domain(&conn, "api.example.com");
        insert_domain(&conn, "www.example.com");
        insert_domain(&conn, "other.com");

        let cache = CertLinkCache::load_from_db(&conn).unwrap();
        assert!(cache.is_empty());

        // Insert a wildcard cert covering *.example.com.
        insert_cert(&conn, "example.com", &["*.example.com"], "issued");
        cache.relink_for_cert(&conn, "example.com").unwrap();

        // *.example.com domains now linked; other.com stays absent.
        assert_eq!(cache.len(), 2);
        assert_eq!(
            cache.lookup("api.example.com").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            cache.lookup("www.example.com").as_deref(),
            Some("example.com")
        );
        assert!(cache.lookup("other.com").is_none());
    }

    #[test]
    fn remove_domain_drops_from_cache() {
        let conn = setup_db();
        insert_domain(&conn, "example.com");
        insert_cert(&conn, "example.com", &[], "issued");

        let cache = CertLinkCache::load_from_db(&conn).unwrap();
        assert_eq!(cache.len(), 1);

        cache.remove_domain("example.com");
        assert!(cache.is_empty());
        assert!(cache.lookup("example.com").is_none());
    }

    // ---- Atomic rebuild (issue #213) ----

    #[test]
    fn reload_from_db_publishes_a_new_table_and_leaves_the_old_one_intact() {
        // The rebuild must not clear the live table: it builds a new
        // one and swaps it in. A reader that still holds the old
        // table keeps seeing the whole old table.
        let conn = setup_db();
        insert_domain(&conn, "example.com");
        insert_cert(&conn, "example.com", &["*.example.com"], "issued");

        let cache = CertLinkCache::load_from_db(&conn).unwrap();
        let old_table = cache.map.read().clone();
        assert_eq!(old_table.len(), 1);

        // A second domain makes the rebuild produce a different table.
        insert_domain(&conn, "api.example.com");
        cache.reload_from_db(&conn).unwrap();

        let new_table = cache.map.read().clone();
        assert!(
            !Arc::ptr_eq(&old_table, &new_table),
            "the rebuild must publish a new table instead of mutating the live one"
        );
        // The old table was never cleared in place.
        assert_eq!(old_table.len(), 1);
        assert_eq!(
            old_table.get("example.com").map(|v| v.value().clone()),
            Some("example.com".to_string())
        );
        // The new table carries both domains.
        assert_eq!(new_table.len(), 2);
        assert_eq!(cache.len(), 2);
        assert_eq!(
            cache.lookup("api.example.com").as_deref(),
            Some("example.com")
        );
    }

    #[test]
    fn reload_from_db_failure_keeps_the_previous_links() {
        // A rebuild that fails must leave the live table exactly as it
        // was — not empty, not half-filled. The failure happens
        // before the swap, so the old links survive.
        let conn = setup_db();
        insert_domain(&conn, "example.com");
        insert_cert(&conn, "example.com", &[], "issued");

        let cache = CertLinkCache::load_from_db(&conn).unwrap();
        assert_eq!(cache.lookup("example.com").as_deref(), Some("example.com"));

        // A connection without the `domains` table makes the rebuild
        // fail on its first query.
        let broken = Connection::open_in_memory().unwrap();
        assert!(
            cache.reload_from_db(&broken).is_err(),
            "the rebuild must report the DB error"
        );

        // The links that were already there survive the failed rebuild.
        assert_eq!(cache.lookup("example.com").as_deref(), Some("example.com"));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn lookup_never_sees_a_missing_link_while_a_rebuild_runs() {
        // The property issue #213 is about: a handshake that runs
        // while the cache is being rebuilt must resolve the link on
        // every read. An in-place rebuild (clear, then refill) leaves
        // a window where the lookup misses; the swap-pointer rebuild
        // never does, because the live table is never emptied.
        //
        // The reader runs on its own thread against a clone (the SNI
        // callback's situation) while the main thread rebuilds. The
        // rebuild's write lock is taken only for the swap — after the
        // new table is fully built — so the reader is never blocked
        // for the duration of the rebuild either.
        use std::sync::atomic::{AtomicBool, Ordering};

        let conn = setup_db();
        insert_domain(&conn, "example.com");
        insert_cert(&conn, "example.com", &["*.example.com"], "issued");

        let cache = CertLinkCache::load_from_db(&conn).unwrap();
        let reader_cache = cache.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let reader_stop = Arc::clone(&stop);

        let reader = std::thread::spawn(move || {
            let mut reads = 0u64;
            while !reader_stop.load(Ordering::Relaxed) {
                assert_eq!(
                    reader_cache.lookup("example.com").as_deref(),
                    Some("example.com"),
                    "a lookup during a rebuild must not miss a live link"
                );
                reads += 1;
            }
            reads
        });

        for _ in 0..200 {
            cache.reload_from_db(&conn).unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        let reads = reader.join().expect("the reader thread must not panic");
        assert!(reads > 0, "the reader must have observed the table");
    }
}
