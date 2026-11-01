//! Provider configuration + origin guard integration tests (issue #122).
//!
//! Covers four layers:
//!   - the DB layer (`site_providers`, `site_origin_allow`,
//!     `provider_ip_lists`),
//!   - the index build (a site's effective provider is resolved
//!     against the global defaults),
//!   - the background fetch (conditional requests, persistence, and
//!     the never-wipe-on-failure rule),
//!   - the admin round-trips (the site form and the settings form).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use rusqlite::Connection;
use tempfile::TempDir;

use pangolin_core::cidr::CidrSet;
use pangolin_core::db;
use pangolin_core::provider::{
    GlobalProviderConfig, ProviderIpList, ProviderKey, SiteProviderOverride,
};
use pangolin_core::{App, CertManager};

/// A fresh database in a temp dir, migrated to the latest schema.
fn temp_conn() -> (TempDir, Connection) {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db");
    let mut conn = Connection::open(&db_path).unwrap();
    db::migrate(&mut conn).unwrap();
    (dir, conn)
}

/// An `App` backed by a fresh database in `dir`.
fn make_test_app(dir: &TempDir) -> Arc<App> {
    let db_path = dir.path().join("test.db");
    {
        let mut conn = Connection::open(&db_path).unwrap();
        db::migrate(&mut conn).unwrap();
    }
    Arc::new(
        App::new(
            &db_path,
            pangolin_core::config::Config::default(),
            CertManager::default(),
        )
        .unwrap(),
    )
}

// ── DB layer ───────────────────────────────────────────────────────

#[test]
fn the_site_provider_override_round_trips() {
    let (_dir, conn) = temp_conn();
    assert!(db::list_site_providers(&conn).unwrap().is_empty());

    let over = SiteProviderOverride {
        provider: Some(ProviderKey::Cloudflare),
        client_ip_header: Some("X-Site-IP".into()),
        country_header: None,
        region_header: Some("X-Site-Region".into()),
        origin_guard_enabled: Some(true),
    };
    db::upsert_site_provider(&conn, "site-a", &over).unwrap();

    let back = db::get_site_provider(&conn, "site-a").unwrap().unwrap();
    assert_eq!(back, over);

    let all = db::list_site_providers(&conn).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all["site-a"], over);

    // Upserting again overwrites — the row is keyed by the site name.
    let over2 = SiteProviderOverride {
        provider: Some(ProviderKey::Direct),
        ..Default::default()
    };
    db::upsert_site_provider(&conn, "site-a", &over2).unwrap();
    assert_eq!(db::list_site_providers(&conn).unwrap().len(), 1);
    assert_eq!(
        db::get_site_provider(&conn, "site-a").unwrap().unwrap(),
        over2
    );

    assert!(db::delete_site_provider(&conn, "site-a").unwrap());
    assert!(db::get_site_provider(&conn, "site-a").unwrap().is_none());
}

#[test]
fn an_unset_provider_override_inherits_the_global_default() {
    let (_dir, conn) = temp_conn();
    // A row whose columns are all NULL is equivalent to no row at
    // all: the site inherits every global default.
    let over = SiteProviderOverride::default();
    db::upsert_site_provider(&conn, "site-a", &over).unwrap();

    let global = GlobalProviderConfig {
        provider: ProviderKey::Cloudflare,
        client_ip_header: Some("X-Global-IP".into()),
        country_header: None,
        region_header: None,
        origin_guard_enabled: true,
    };
    let resolved = global.resolve(&db::get_site_provider(&conn, "site-a").unwrap().unwrap());
    assert_eq!(resolved.provider, ProviderKey::Cloudflare);
    assert_eq!(resolved.client_ip_header.as_deref(), Some("X-Global-IP"));
    assert!(resolved.origin_guard_enabled);
}

#[test]
fn the_manual_ranges_round_trip() {
    let (_dir, conn) = temp_conn();
    assert!(db::list_site_origin_allow(&conn).unwrap().is_empty());

    let ranges = vec!["203.0.113.0/24".to_string(), "198.51.100.7".to_string()];
    db::replace_site_origin_allow(&conn, "site-a", &ranges).unwrap();

    let back = db::list_site_origin_allow(&conn).unwrap();
    // The rows are read back in their canonical order.
    let mut expected = ranges.clone();
    expected.sort();
    assert_eq!(back["site-a"], expected);

    // Replacing drops the previous set — removing a range from the
    // form actually removes it.
    db::replace_site_origin_allow(&conn, "site-a", &["10.0.0.0/8".to_string()]).unwrap();
    let back = db::list_site_origin_allow(&conn).unwrap();
    assert_eq!(back["site-a"], vec!["10.0.0.0/8".to_string()]);
}

#[test]
fn the_provider_range_list_round_trips() {
    let (_dir, conn) = temp_conn();
    assert!(db::list_provider_ip_lists(&conn).unwrap().is_empty());

    let row = ProviderIpList {
        provider: ProviderKey::Cloudflare,
        url: "https://www.cloudflare.com/ips-v4".into(),
        cidrs: "173.245.48.0/20\n103.21.244.0/22".into(),
        fetched_at: Utc::now(),
        etag: Some("\"abc\"".into()),
        last_modified: Some("Mon, 01 Jan 2026 00:00:00 GMT".into()),
        status: "ok".into(),
        last_error: None,
    };
    db::upsert_provider_ip_list(&conn, &row).unwrap();

    let back = db::list_provider_ip_lists(&conn).unwrap();
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].provider, ProviderKey::Cloudflare);
    assert_eq!(back[0].cidrs, row.cidrs);
    assert_eq!(back[0].etag.as_deref(), Some("\"abc\""));

    // Each URL keeps its own row — the key is (provider, url).
    let row2 = ProviderIpList {
        url: "https://www.cloudflare.com/ips-v6".into(),
        ..row.clone()
    };
    db::upsert_provider_ip_list(&conn, &row2).unwrap();
    assert_eq!(db::list_provider_ip_lists(&conn).unwrap().len(), 2);
}

// ── Migrations ─────────────────────────────────────────────────────

#[test]
fn the_migration_applies_to_a_fresh_database() {
    // R17: a fresh database applies every migration in order.
    let (_dir, conn) = temp_conn();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM refinery_schema_history", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(count, 10, "V1..V10 must be applied");

    // The renamed column and the new tables are there.
    let cols: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('system_config')")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert!(cols.iter().any(|c| c == "provider"), "{cols:?}");
    assert!(!cols.iter().any(|c| c == "frontend_mode"), "{cols:?}");
    assert!(cols.iter().any(|c| c == "origin_guard_enabled"), "{cols:?}");

    for table in ["site_providers", "site_origin_allow", "provider_ip_lists"] {
        let n: i64 = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='{table}'"
                ),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "the {table} table must exist");
    }
}

#[test]
fn the_migration_renames_the_column_and_keeps_the_check_constraint() {
    // R17: the CHECK constraint travels with the rename, so the
    // allowed provider values are unchanged.
    let (_dir, conn) = temp_conn();
    let err = conn
        .execute(
            "UPDATE system_config SET provider = 'bogus' WHERE id = 1",
            [],
        )
        .expect_err("the CHECK constraint must reject an unknown provider");
    assert!(
        format!("{err}").contains("CHECK constraint failed"),
        "{err}"
    );

    // The seeded row survived the rename.
    let cfg = db::get_system_config(&conn).unwrap();
    assert_eq!(cfg.provider, ProviderKey::Direct);
}

#[test]
fn the_migration_applies_to_an_existing_database() {
    // R17: an existing database migrates in place. The database is
    // built at V9 (the version before this change) and then upgraded.
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db");
    {
        let mut conn = Connection::open(&db_path).unwrap();
        pangolin_core::embedded_migrations::run_migrations_up_to(&mut conn, 9).unwrap();

        // The pre-change schema: the old column name, no provider
        // tables.
        let cols: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('system_config')")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(cols.iter().any(|c| c == "frontend_mode"), "{cols:?}");
        assert!(!cols.iter().any(|c| c == "provider"), "{cols:?}");

        // A row the migration has to carry over.
        conn.execute(
            "UPDATE system_config SET frontend_mode = 'cloudflare' WHERE id = 1",
            [],
        )
        .unwrap();
    }

    // The upgrade runs the full migration set.
    {
        let mut conn = Connection::open(&db_path).unwrap();
        db::migrate(&mut conn).unwrap();
    }

    let conn = Connection::open(&db_path).unwrap();
    let cols: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('system_config')")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert!(cols.iter().any(|c| c == "provider"), "{cols:?}");
    assert!(!cols.iter().any(|c| c == "frontend_mode"), "{cols:?}");

    // The row survived the rename, with its value.
    let cfg = db::get_system_config(&conn).unwrap();
    assert_eq!(cfg.provider, ProviderKey::Cloudflare);

    // And the new tables are there.
    for table in ["site_providers", "site_origin_allow", "provider_ip_lists"] {
        let n: i64 = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='{table}'"
                ),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "the {table} table must exist");
    }
}

#[test]
fn deleting_a_site_removes_its_provider_configuration() {
    // A leftover row would resurrect a stale origin guard (and its
    // stale ranges) when a site with the same name is created again.
    let (_dir, conn) = temp_conn();
    db::upsert_site_provider(
        &conn,
        "site-a",
        &SiteProviderOverride {
            provider: Some(ProviderKey::CustomLb),
            origin_guard_enabled: Some(true),
            ..Default::default()
        },
    )
    .unwrap();
    db::replace_site_origin_allow(&conn, "site-a", &["203.0.113.0/24".to_string()]).unwrap();

    // The site itself.
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO sites (name, backend, enabled, host_mode, created_at, updated_at)
         VALUES ('site-a', 'http://127.0.0.1:8080', 1, 'passthrough', ?1, ?1)",
        rusqlite::params![now],
    )
    .unwrap();

    assert!(db::delete_site(&conn, "site-a").unwrap());

    assert!(db::get_site_provider(&conn, "site-a").unwrap().is_none());
    assert!(db::list_site_origin_allow(&conn).unwrap().is_empty());
}

// ── Index build ────────────────────────────────────────────────────

#[test]
fn a_site_without_a_provider_row_inherits_the_global_provider() {
    let (_dir, conn) = temp_conn();
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO sites (name, backend, enabled, host_mode, created_at, updated_at)
         VALUES ('site-a', 'http://127.0.0.1:8080', 1, 'passthrough', ?1, ?1)",
        rusqlite::params![now],
    )
    .unwrap();

    let global = GlobalProviderConfig {
        provider: ProviderKey::Cloudflare,
        ..Default::default()
    };
    let indexes = pangolin_core::index::Indexes::build_with_providers(
        vec![pangolin_core::types::Site {
            name: "site-a".into(),
            backend: "http://127.0.0.1:8080".into(),
            enabled: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            host_mode: pangolin_core::types::HostMode::Passthrough,
            host_custom: None,
            domain_count: 0,
        }],
        vec![],
        &HashMap::new(),
        &[],
        &HashMap::new(),
        &HashMap::new(),
        &global,
    );
    let cfg = indexes
        .providers
        .get("site-a")
        .expect("the site has a provider entry");
    assert_eq!(cfg.provider, ProviderKey::Cloudflare);
}

#[test]
fn a_site_with_a_provider_row_overrides_the_global_provider() {
    let global = GlobalProviderConfig {
        provider: ProviderKey::Cloudflare,
        ..Default::default()
    };
    let overrides = HashMap::from([(
        "site-a".to_string(),
        SiteProviderOverride {
            provider: Some(ProviderKey::Direct),
            ..Default::default()
        },
    )]);
    let ranges = HashMap::from([("site-a".to_string(), vec!["203.0.113.0/24".to_string()])]);

    let site = pangolin_core::types::Site {
        name: "site-a".into(),
        backend: "http://127.0.0.1:8080".into(),
        enabled: true,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        host_mode: pangolin_core::types::HostMode::Passthrough,
        host_custom: None,
        domain_count: 0,
    };
    let indexes = pangolin_core::index::Indexes::build_with_providers(
        vec![site],
        vec![],
        &HashMap::new(),
        &[],
        &overrides,
        &ranges,
        &global,
    );
    let cfg = indexes
        .providers
        .get("site-a")
        .expect("the site has a provider entry");
    assert_eq!(cfg.provider, ProviderKey::Direct);
    assert!(cfg.manual_ranges.contains("203.0.113.7".parse().unwrap()));
}

// ── Background fetch ───────────────────────────────────────────────

/// A minimal HTTP/1.1 stub server.
///
/// It answers every request with the same canned response — enough
/// for the conditional-request tests, which need to observe the
/// `If-None-Match` header the fetch sends.
struct StubServer {
    addr: String,
    /// The response the server answers with. Switchable so the same
    /// URL can answer `200` first and `500` later — the row is keyed
    /// by (provider, url), so the test must not change the URL.
    response: Arc<std::sync::Mutex<(String, String)>>,
    handle: tokio::task::JoinHandle<()>,
}

impl StubServer {
    async fn start(status_line: &'static str, body: &'static str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let response = Arc::new(std::sync::Mutex::new((
            status_line.to_string(),
            body.to_string(),
        )));
        let for_task = response.clone();
        let handle = tokio::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let response = for_task.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 4096];
                    let n = match stream.read(&mut buf).await {
                        Ok(n) if n > 0 => n,
                        _ => return,
                    };
                    let (status_line, body) = response.lock().unwrap().clone();
                    let reply = format!(
                        "{status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(reply.as_bytes()).await;
                    let _ = stream.flush().await;
                });
            }
        });
        Self {
            addr,
            response,
            handle,
        }
    }

    /// Change the response the server answers with.
    fn set_response(&self, status_line: &str, body: &str) {
        *self.response.lock().unwrap() = (status_line.to_string(), body.to_string());
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }
}

impl Drop for StubServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// A reqwest client that talks plain HTTP to the stub.
fn stub_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("build stub client")
}

#[tokio::test]
async fn a_fetch_stores_the_ranges_and_they_become_effective() {
    // R8: the fetched ranges are stored and become effective without
    // a restart.
    let dir = TempDir::new().unwrap();
    let app = make_test_app(&dir);
    let stub = StubServer::start("HTTP/1.1 200 OK", "173.245.48.0/20\n103.21.244.0/22\n").await;
    let url = stub.url("/ips-v4");

    let outcome =
        ngx::provider_ip::fetch_range_list(&app, &stub_client(), ProviderKey::Cloudflare, &url)
            .await
            .expect("the fetch must succeed");
    assert_eq!(
        outcome,
        ngx::provider_ip::FetchOutcome::Updated { ranges: 2 }
    );

    // The ranges are persisted…
    {
        let db = app.db.lock().await;
        let rows = db::list_provider_ip_lists(&db).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].provider, ProviderKey::Cloudflare);
        assert_eq!(rows[0].status, "ok");
        assert!(rows[0].cidrs.contains("173.245.48.0/20"));
    }

    // …and in force, without a restart.
    let in_force = app.provider_ranges.read();
    let set = in_force
        .get(&ProviderKey::Cloudflare)
        .expect("the provider has a range set");
    assert_eq!(set.len(), 2);
    assert!(set.contains("173.245.48.5".parse().unwrap()));
    assert!(!set.contains("192.0.2.1".parse().unwrap()));
}

#[tokio::test]
async fn a_304_response_keeps_the_stored_ranges() {
    // R8: a `304 Not Modified` keeps the stored ranges.
    let dir = TempDir::new().unwrap();
    let app = make_test_app(&dir);
    let stub = StubServer::start("HTTP/1.1 304 Not Modified", "").await;
    let url = stub.url("/ips-v4");

    // A first, successful fetch.
    let stub = StubServer::start("HTTP/1.1 200 OK", "173.245.48.0/20\n").await;
    let url = stub.url("/ips-v4");
    ngx::provider_ip::fetch_range_list(&app, &stub_client(), ProviderKey::Cloudflare, &url)
        .await
        .expect("the first fetch must succeed");

    // The second fetch answers 304 — the same URL, so the stored
    // row (and its validators) is the one the conditional request
    // refers to.
    stub.set_response("HTTP/1.1 304 Not Modified", "");
    let outcome =
        ngx::provider_ip::fetch_range_list(&app, &stub_client(), ProviderKey::Cloudflare, &url)
            .await
            .expect("the conditional fetch must succeed");
    assert_eq!(outcome, ngx::provider_ip::FetchOutcome::NotModified);

    // The stored ranges are still the ones of the first fetch.
    let in_force = app.provider_ranges.read();
    let set = in_force
        .get(&ProviderKey::Cloudflare)
        .expect("the provider has a range set");
    assert_eq!(set.len(), 1);
    assert!(set.contains("173.245.48.5".parse().unwrap()));
}

#[tokio::test]
async fn a_failed_fetch_keeps_the_stored_ranges_and_records_the_error() {
    // R9: a failed fetch never removes or replaces the stored ranges.
    let dir = TempDir::new().unwrap();
    let app = make_test_app(&dir);

    let stub = StubServer::start("HTTP/1.1 200 OK", "173.245.48.0/20\n").await;
    let url = stub.url("/ips-v4");
    ngx::provider_ip::fetch_range_list(&app, &stub_client(), ProviderKey::Cloudflare, &url)
        .await
        .expect("the first fetch must succeed");

    // The second fetch fails with an HTTP error.
    stub.set_response("HTTP/1.1 500 Internal Server Error", "boom");
    let err =
        ngx::provider_ip::fetch_range_list(&app, &stub_client(), ProviderKey::Cloudflare, &url)
            .await
            .expect_err("the fetch must fail");
    assert!(err.contains("500"), "{err}");

    // The stored ranges are still in force…
    let in_force = app.provider_ranges.read();
    let set = in_force
        .get(&ProviderKey::Cloudflare)
        .expect("the provider has a range set");
    assert_eq!(set.len(), 1);
    assert!(set.contains("173.245.48.5".parse().unwrap()));

    // …and the failure is recorded.
    let db = app.db.lock().await;
    let rows = db::list_provider_ip_lists(&db).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "error");
    assert!(rows[0].last_error.is_some(), "{rows:?}");
    // The stored ranges were not replaced by the failed fetch.
    assert!(rows[0].cidrs.contains("173.245.48.0/20"));
}

#[tokio::test]
async fn a_malformed_body_keeps_the_stored_ranges() {
    // A truncated or substituted body must not silently shrink the
    // allowlist.
    let dir = TempDir::new().unwrap();
    let app = make_test_app(&dir);

    let stub = StubServer::start("HTTP/1.1 200 OK", "173.245.48.0/20\n").await;
    let url = stub.url("/ips-v4");
    ngx::provider_ip::fetch_range_list(&app, &stub_client(), ProviderKey::Cloudflare, &url)
        .await
        .expect("the first fetch must succeed");

    stub.set_response("HTTP/1.1 200 OK", "not-a-cidr\n");
    let err =
        ngx::provider_ip::fetch_range_list(&app, &stub_client(), ProviderKey::Cloudflare, &url)
            .await
            .expect_err("the fetch must fail");
    assert!(err.contains("not-a-cidr"), "{err}");

    let in_force = app.provider_ranges.read();
    let set = in_force
        .get(&ProviderKey::Cloudflare)
        .expect("the provider has a range set");
    assert_eq!(set.len(), 1, "the stored ranges must stay in force");
}

#[tokio::test]
async fn an_empty_body_keeps_the_stored_ranges() {
    // A truncated response (an empty body) must not empty the
    // allowlist — that would silently switch the guard off.
    let dir = TempDir::new().unwrap();
    let app = make_test_app(&dir);

    let stub = StubServer::start("HTTP/1.1 200 OK", "173.245.48.0/20\n").await;
    let url = stub.url("/ips-v4");
    ngx::provider_ip::fetch_range_list(&app, &stub_client(), ProviderKey::Cloudflare, &url)
        .await
        .expect("the first fetch must succeed");

    stub.set_response("HTTP/1.1 200 OK", "");
    let err =
        ngx::provider_ip::fetch_range_list(&app, &stub_client(), ProviderKey::Cloudflare, &url)
            .await
            .expect_err("an empty body must fail the fetch");
    assert!(err.contains("empty"), "{err}");

    // The stored ranges are still in force…
    let in_force = app.provider_ranges.read();
    let set = in_force
        .get(&ProviderKey::Cloudflare)
        .expect("the provider has a range set");
    assert_eq!(set.len(), 1, "the stored ranges must stay in force");
    assert!(set.contains("173.245.48.5".parse().unwrap()));

    // …and the failure is recorded.
    let db = app.db.lock().await;
    let rows = db::list_provider_ip_lists(&db).unwrap();
    assert_eq!(rows[0].status, "error");
    assert!(rows[0].cidrs.contains("173.245.48.0/20"));
}

#[tokio::test]
async fn the_refresh_interval_is_clamped_to_at_least_one_second() {
    // A zero interval would make the refresh loop spin at full
    // speed.
    let service = ngx::provider_ip::ProviderIpService::new(Duration::from_secs(0)).unwrap();
    assert_eq!(service.refresh_interval(), Duration::from_secs(1));
    let service = ngx::provider_ip::ProviderIpService::new(Duration::from_secs(3600)).unwrap();
    assert_eq!(service.refresh_interval(), Duration::from_secs(3600));
}

#[tokio::test]
async fn the_persisted_ranges_are_loaded_into_memory_on_start() {
    // The guard has to work before the first fetch completes.
    let dir = TempDir::new().unwrap();
    let app = make_test_app(&dir);
    {
        let db = app.db.lock().await;
        db::upsert_provider_ip_list(
            &db,
            &ProviderIpList {
                provider: ProviderKey::Cloudflare,
                url: "https://www.cloudflare.com/ips-v4".into(),
                cidrs: "173.245.48.0/20\n".into(),
                fetched_at: Utc::now(),
                etag: None,
                last_modified: None,
                status: "ok".into(),
                last_error: None,
            },
        )
        .unwrap();
    }

    let service = ngx::provider_ip::ProviderIpService::new(Duration::from_secs(3600)).unwrap();
    service.load_persisted(&app).await;

    let in_force = app.provider_ranges.read();
    let set = in_force
        .get(&ProviderKey::Cloudflare)
        .expect("the provider has a range set");
    assert_eq!(set.len(), 1);
    assert!(set.contains("173.245.48.5".parse().unwrap()));
}

// ── The guard's allowlist ──────────────────────────────────────────

#[test]
fn the_allowlist_is_the_union_of_the_provider_ranges_and_the_manual_ones() {
    // The union is checked by membership, not by materialising it —
    // the guard runs on the request hot path.
    use pangolin_core::provider::SiteProviderConfig;
    let mut cfg = SiteProviderConfig {
        provider: ProviderKey::Cloudflare,
        origin_guard_enabled: true,
        ..Default::default()
    };
    cfg.manual_ranges.insert("198.51.100.0/24").unwrap();

    let mut provider_ranges = CidrSet::new();
    provider_ranges.insert("203.0.113.0/24").unwrap();

    // A peer inside the provider's ranges is allowed…
    assert!(cfg.allows_peer(Some("203.0.113.7".parse().unwrap()), &provider_ranges));
    // …and so is one inside the site's manual ranges.
    assert!(cfg.allows_peer(Some("198.51.100.7".parse().unwrap()), &provider_ranges));
    // Everything else is rejected.
    assert!(!cfg.allows_peer(Some("192.0.2.7".parse().unwrap()), &provider_ranges));
    // An empty allowlist fails open.
    assert!(cfg.has_empty_allowlist(&CidrSet::new()) == false);
    let mut empty = SiteProviderConfig::default();
    empty.manual_ranges = CidrSet::new();
    assert!(empty.has_empty_allowlist(&CidrSet::new()));
    assert!(empty.allows_peer(Some("192.0.2.7".parse().unwrap()), &CidrSet::new()));
}

#[test]
fn the_effective_allowlist_is_the_provider_ranges_plus_the_manual_ones() {
    // R7: the union of both.
    use pangolin_core::provider::SiteProviderConfig;
    let mut cfg = SiteProviderConfig {
        provider: ProviderKey::Cloudflare,
        origin_guard_enabled: true,
        ..Default::default()
    };
    cfg.manual_ranges.insert("198.51.100.0/24").unwrap();

    let mut provider_ranges = CidrSet::new();
    provider_ranges.insert("203.0.113.0/24").unwrap();

    assert!(cfg.allows_peer(Some("203.0.113.7".parse().unwrap()), &provider_ranges));
    assert!(cfg.allows_peer(Some("198.51.100.7".parse().unwrap()), &provider_ranges));
    assert!(!cfg.allows_peer(Some("192.0.2.7".parse().unwrap()), &provider_ranges));
}

// ── Admin round-trips (R5) ─────────────────────────────────────────

/// The global provider is stored by the settings form and read back.
#[tokio::test]
async fn the_settings_form_stores_the_global_provider() {
    let ngx = crate::harness::NgxProcess::start(crate::harness::init_pangolin_db).await;
    let client = crate::admin_harness::AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let csrf = client
        .csrf_token(&client.get("/settings").await.unwrap().text().await.unwrap())
        .expect("csrf on /settings");

    let resp = client
        .post_form("/settings", &[("provider", "cloudflare"), ("_csrf", &csrf)])
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "a successful POST should redirect to /settings"
    );

    // The form reflects the change…
    let body = client.get("/settings").await.unwrap().text().await.unwrap();
    assert!(
        body.contains(r#"value="cloudflare" checked"#),
        "cloudflare should be pre-selected after the POST; got:\n{body}"
    );

    // …and so does the JSON API.
    let resp = client.get("/api/system/config").await.unwrap();
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["provider"], serde_json::json!("cloudflare"));
}

/// The site form stores the site's provider configuration, and a site
/// without one uses the global default.
#[tokio::test]
async fn the_site_form_stores_the_provider_configuration() {
    let ngx = crate::harness::NgxProcess::start(crate::harness::init_pangolin_db).await;
    let client = crate::admin_harness::AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let csrf = client
        .csrf_token(
            &client
                .get("/sites/new")
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
        )
        .expect("csrf on /sites/new");

    let resp = client
        .post_form(
            "/sites/new",
            &[
                ("name", "guarded"),
                ("backend", "http://127.0.0.1:8080"),
                ("provider", "custom_lb"),
                ("client_ip_header", "X-Site-IP"),
                ("origin_guard", "on"),
                ("origin_allow", "203.0.113.0/24"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "a successful POST should redirect to /sites"
    );

    // The configuration is persisted…
    {
        let conn = Connection::open(ngx.db_path()).unwrap();
        let over = db::get_site_provider(&conn, "guarded").unwrap().unwrap();
        assert_eq!(over.provider, Some(ProviderKey::CustomLb));
        assert_eq!(over.client_ip_header.as_deref(), Some("X-Site-IP"));
        assert_eq!(over.origin_guard_enabled, Some(true));
        let ranges = db::list_site_origin_allow(&conn).unwrap();
        assert_eq!(ranges["guarded"], vec!["203.0.113.0/24".to_string()]);
    }

    // …and the site form shows it back.
    let body = client
        .get("/sites/edit?name=guarded")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        body.contains(r#"<option value="custom_lb" selected>"#),
        "the site form must show the site's own provider; got:\n{body}"
    );
    assert!(
        body.contains(r#"value="X-Site-IP""#),
        "the site form must show the site's own header name; got:\n{body}"
    );
}

/// An update that does not carry the manual ranges must not wipe the
/// saved ones: for a `custom_lb` site they are the only thing the
/// origin guard has.
#[tokio::test]
async fn an_update_without_the_manual_ranges_keeps_the_saved_ones() {
    let ngx = crate::harness::NgxProcess::start(crate::harness::init_pangolin_db).await;
    let client = crate::admin_harness::AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let csrf = client
        .csrf_token(
            &client
                .get("/sites/new")
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
        )
        .expect("csrf on /sites/new");
    let resp = client
        .post_form(
            "/sites/new",
            &[
                ("name", "guarded"),
                ("backend", "http://127.0.0.1:8080"),
                ("provider", "custom_lb"),
                ("origin_guard", "on"),
                ("origin_allow", "203.0.113.0/24"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 302);

    // An update that carries the provider but not the ranges.
    let csrf = client
        .csrf_token(
            &client
                .get("/sites/edit?name=guarded")
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
        )
        .expect("csrf on /sites/edit");
    let resp = client
        .post_form(
            "/sites/edit?name=guarded",
            &[
                ("name", "guarded"),
                ("backend", "http://127.0.0.1:8080"),
                ("provider", "custom_lb"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 302);

    let conn = Connection::open(ngx.db_path()).unwrap();
    let ranges = db::list_site_origin_allow(&conn).unwrap();
    assert_eq!(
        ranges["guarded"],
        vec!["203.0.113.0/24".to_string()],
        "the saved manual ranges must survive an update that omits the field"
    );
}

/// A rejected submission keeps the operator's typed provider fields,
/// exactly like the rate-limit fields.
#[tokio::test]
async fn a_rejected_submission_keeps_the_typed_provider_fields() {
    let ngx = crate::harness::NgxProcess::start(crate::harness::init_pangolin_db).await;
    let client = crate::admin_harness::AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let csrf = client
        .csrf_token(
            &client
                .get("/sites/new")
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
        )
        .expect("csrf on /sites/new");

    let resp = client
        .post_form(
            "/sites/new",
            &[
                ("name", "guarded"),
                ("backend", "http://127.0.0.1:8080"),
                ("provider", "custom_lb"),
                ("client_ip_header", "X-Site-IP"),
                ("origin_guard", "on"),
                ("origin_allow", "not-a-cidr"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "a malformed range must re-render the form, not redirect"
    );
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("not-a-cidr"),
        "the error must name the offending value; got:\n{body}"
    );
    // The typed values are still in the form…
    assert!(
        body.contains(r#"value="custom_lb" selected>"#),
        "the provider select must keep the typed value; got:\n{body}"
    );
    assert!(
        body.contains(r#"value="X-Site-IP""#),
        "the header name must keep the typed value; got:\n{body}"
    );
    assert!(
        body.contains(r#"<option value="on" selected>"#),
        "the guard tri-state must keep the typed value; got:\n{body}"
    );
    assert!(
        body.contains("not-a-cidr"),
        "the manual ranges must keep the typed value; got:\n{body}"
    );
}

/// The global origin guard cannot be switched on for a provider that
/// publishes no source-IP ranges: it would fail open.
#[tokio::test]
async fn the_global_guard_cannot_be_enabled_without_published_ranges() {
    let ngx = crate::harness::NgxProcess::start(crate::harness::init_pangolin_db).await;
    let client = crate::admin_harness::AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let csrf = client
        .csrf_token(&client.get("/settings").await.unwrap().text().await.unwrap())
        .expect("csrf on /settings");

    // `custom_lb` publishes no ranges.
    let resp = client
        .post_form(
            "/settings",
            &[
                ("provider", "custom_lb"),
                ("origin_guard_enabled", "true"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "the guard must be refused for a provider with no ranges"
    );
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("publishes no"),
        "the error must explain why; got:\n{body}"
    );

    // The stored config is unchanged.
    let resp = client.get("/api/system/config").await.unwrap();
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["origin_guard_enabled"], serde_json::json!(false));
}

/// A settings POST that does not carry the guard switch keeps the
/// stored value — an API client must not switch the global guard off
/// by omission.
#[tokio::test]
async fn a_settings_post_without_the_guard_switch_keeps_the_stored_value() {
    let ngx = crate::harness::NgxProcess::start(crate::harness::init_pangolin_db).await;
    let client = crate::admin_harness::AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let csrf = client
        .csrf_token(&client.get("/settings").await.unwrap().text().await.unwrap())
        .expect("csrf on /settings");
    let resp = client
        .post_form(
            "/settings",
            &[
                ("provider", "cloudflare"),
                ("origin_guard_enabled", "true"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 302);

    // Now a POST without the field.
    let csrf = client
        .csrf_token(&client.get("/settings").await.unwrap().text().await.unwrap())
        .expect("csrf on /settings");
    let resp = client
        .post_form("/settings", &[("provider", "cloudflare"), ("_csrf", &csrf)])
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 302);

    let resp = client.get("/api/system/config").await.unwrap();
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        json["origin_guard_enabled"],
        serde_json::json!(true),
        "an absent field must not switch the global guard off"
    );
}

/// A malformed manual range is rejected with a message naming the
/// offending value, and nothing is stored.
#[tokio::test]
async fn a_malformed_manual_range_is_rejected_and_not_stored() {
    let ngx = crate::harness::NgxProcess::start(crate::harness::init_pangolin_db).await;
    let client = crate::admin_harness::AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let csrf = client
        .csrf_token(
            &client
                .get("/sites/new")
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
        )
        .expect("csrf on /sites/new");

    let resp = client
        .post_form(
            "/sites/new",
            &[
                ("name", "bad-range"),
                ("backend", "http://127.0.0.1:8080"),
                ("provider", "custom_lb"),
                ("origin_allow", "not-a-cidr"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "a malformed range must re-render the form, not redirect"
    );
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("not-a-cidr"),
        "the error must name the offending value; got:\n{body}"
    );

    // Nothing was stored.
    let conn = Connection::open(ngx.db_path()).unwrap();
    assert!(db::get_site_provider(&conn, "bad-range").unwrap().is_none());
    assert!(db::list_site_origin_allow(&conn).unwrap().is_empty());
}
