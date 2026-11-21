//! Reload indexes integration tests.
//!
//! Covers: tests/E2E_PLAN.md → reload_indexes_triggered
//! Directly tests App::reload_indexes() method behavior.

use chrono::Utc;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

use pangolin_core::db;
use pangolin_core::types::{Domain, HostMode, Site};
use pangolin_core::{App, CertManager, Indexes};

fn make_cert_manager(cert_dir: &Path) -> CertManager {
    CertManager::new(
        cert_dir.to_path_buf(),
        None,
        String::new(),
        30,
        24,
        "ecdsa".into(),
    )
}

fn make_test_app(dir: &tempfile::TempDir) -> Arc<App> {
    let db_path = dir.path().join("test.db");
    let mut conn = Connection::open(&db_path).unwrap();
    db::migrate(&mut conn).unwrap();

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
        let idx = app.indexes.read().clone();
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
        auto_issue: false,
        dns_provider: None,
        challenge_kind: None,
        created_at: Utc::now(),
    };
    db::upsert_domain(&*app.db.lock().await, &domain).unwrap();

    // Call reload_indexes()
    app.reload_indexes().await;

    // Verify site is now routable by domain
    {
        let idx = app.indexes.read().clone();
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
        let idx = app.indexes.read().clone();
        assert!(pangolin_core::index::lookup_site(&idx, "mydomain.example.com").is_none());
    }

    // Add domain
    let domain = Domain {
        domain: "mydomain.example.com".into(),
        site_name: "routing-site".into(),
        enabled: true,
        auto_issue: false,
        dns_provider: None,
        challenge_kind: None,
        created_at: Utc::now(),
    };
    db::upsert_domain(&*app.db.lock().await, &domain).unwrap();
    app.reload_indexes().await;

    // Now routable
    {
        let idx = app.indexes.read().clone();
        let found = pangolin_core::index::lookup_site(&idx, "mydomain.example.com");
        assert!(found.is_some(), "domain should be routable after reload");
        assert_eq!(found.unwrap().name, "routing-site");
    }
}

// `reload_indexes_token_affects_active_state` removed in v2: the
// in-memory `indexes.token` was dropped when tokens were merged into
// `tun`. Tun enable/disable reload behavior is covered by
// `admin_reload_tun` in `tests/src/admin_reload.rs`.

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
    let idx1 = app.indexes.read().clone();
    let site1_name =
        pangolin_core::index::lookup_site(&idx1, "stable-site.com").map(|s| s.name.clone());
    drop(idx1);

    // Reload again — nothing changed
    app.reload_indexes().await;

    let idx2 = app.indexes.read().clone();
    let site2_name =
        pangolin_core::index::lookup_site(&idx2, "stable-site.com").map(|s| s.name.clone());
    assert_eq!(site1_name, site2_name, "index should be unchanged");
}

// ── The routing table and the tunnel classification are one revision ──
//
// Issue #212: `App::reload_indexes` used to publish the routing table
// and the tunnel classification (the set the TLS ALPN callback reads)
// with two independent writes. Between the two writes the routing
// table already knew a site was tunnel-backed while the handshake
// still classified it as direct — the handshake then advertised `h2`
// and the request entered the yamux tunnel path, which is the
// tokio-yamux stream-state race of issue #66.

/// A site with the given backend. The backend is what decides whether
/// the site is tunnel-backed, and therefore what the routing table and
/// the tunnel classification have to agree about.
fn tunnel_site(name: &str, backend: &str) -> Site {
    Site {
        name: name.into(),
        backend: backend.into(),
        enabled: true,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        host_mode: HostMode::Passthrough,
        host_custom: None,
        domain_count: 0,
    }
}

fn domain_of(domain: &str, site_name: &str) -> Domain {
    Domain {
        domain: domain.into(),
        site_name: site_name.into(),
        enabled: true,
        auto_issue: false,
        dns_provider: None,
        challenge_kind: None,
        created_at: Utc::now(),
    }
}

/// The published revision carries the tunnel classification, and the
/// ALPN callback gets that very allocation.
///
/// `Arc::ptr_eq` is the deterministic form of the invariant: the
/// handshake path and the request path read the **same** set, so
/// there is no window in which they could disagree. The assertion
/// runs without any concurrent reload, so the two reads cannot
/// straddle a publication.
#[tokio::test]
async fn published_revision_carries_its_own_tunnel_classification() {
    let dir = TempDir::new().unwrap();
    let app = make_test_app(&dir);

    {
        let conn = app.db.lock().await;
        db::upsert_site(
            &conn,
            &tunnel_site("tun-site", "office:http://10.0.0.1:8080"),
        )
        .unwrap();
        db::upsert_domain(&conn, &domain_of("tun.example.com", "tun-site")).unwrap();
        db::upsert_site(&conn, &tunnel_site("direct-site", "http://127.0.0.1:8080")).unwrap();
        db::upsert_domain(&conn, &domain_of("direct.example.com", "direct-site")).unwrap();
    }
    app.reload_indexes().await;

    let published = app.indexes.read().clone();
    let classification = app.tunnel_classification();

    assert!(
        Arc::ptr_eq(&published.tunnel_domains, &classification),
        "the ALPN callback must read the tunnel classification of the published revision"
    );
    assert!(classification.contains("tun.example.com"));
    assert!(!classification.contains("direct.example.com"));
}

/// Does the published revision classify every domain the way its own
/// routing table routes it?
///
/// `host_matches_set` is what the ALPN callback uses; the site that
/// owns the domain is what the request path routes to. The two have
/// to agree, domain by domain.
fn first_classification_mismatch(revision: &Indexes) -> Option<String> {
    for (domain, site) in &revision.domain {
        let routes_through_tunnel = matches!(
            pangolin_core::parse::parse_backend(&site.backend),
            Ok((ref tun, _)) if !tun.is_empty()
        );
        let classified_as_tunnel =
            pangolin_core::index::host_matches_set(&revision.tunnel_domains, domain);
        if routes_through_tunnel != classified_as_tunnel {
            return Some(format!(
                "{domain}: the routing table sends it {} but the classification says tunnel={classified_as_tunnel}",
                if routes_through_tunnel {
                    "through a tunnel"
                } else {
                    "to a direct backend"
                }
            ));
        }
    }
    None
}

/// A reload publishes the routing table and the tunnel classification
/// together — a reader never sees half of a revision.
///
/// The sampler runs on its own thread and reads the published revision
/// in a tight loop while the reloads flip the site between a tunnel
/// backend and a direct one. Every sample must be self-consistent.
///
/// Under the old two-write publication the reload published the
/// routing table first and the classification last, with the cert-link
/// rebuild in between; a sample taken in that window saw a tunnel
/// site in the routing table and a direct site in the classification.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_publishes_the_routing_table_and_the_tunnel_classification_together() {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let dir = TempDir::new().unwrap();
    let app = make_test_app(&dir);

    // The tunnel site flips between the two backends; the direct site
    // and the bulk site stay put. The bulk domains widen the reload
    // (the cert-link cache is rebuilt per domain), which is what
    // makes the publication window observable.
    const BULK_DOMAINS: usize = 100;
    {
        let conn = app.db.lock().await;
        db::upsert_site(
            &conn,
            &tunnel_site("flip-site", "office:http://10.0.0.1:8080"),
        )
        .unwrap();
        db::upsert_domain(&conn, &domain_of("flip.example.com", "flip-site")).unwrap();
        db::upsert_domain(&conn, &domain_of("*.flip.example.com", "flip-site")).unwrap();
        db::upsert_site(&conn, &tunnel_site("direct-site", "http://127.0.0.1:8080")).unwrap();
        db::upsert_domain(&conn, &domain_of("direct.example.com", "direct-site")).unwrap();
        db::upsert_site(&conn, &tunnel_site("bulk-site", "http://127.0.0.1:8080")).unwrap();
        for i in 0..BULK_DOMAINS {
            db::upsert_domain(
                &conn,
                &domain_of(&format!("bulk-{i}.example.net"), "bulk-site"),
            )
            .unwrap();
        }
    }
    app.reload_indexes().await;

    let stop = Arc::new(AtomicBool::new(false));
    let mismatches = Arc::new(AtomicUsize::new(0));
    let first_mismatch: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let sampler = {
        let app = app.clone();
        let stop = stop.clone();
        let mismatches = mismatches.clone();
        let first_mismatch = first_mismatch.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                // One read of the published revision: the routing
                // table and the classification come from it
                // together.
                let revision = app.indexes.read().clone();
                if let Some(mismatch) = first_classification_mismatch(&revision) {
                    mismatches.fetch_add(1, Ordering::Relaxed);
                    let mut guard = first_mismatch.lock().unwrap();
                    if guard.is_none() {
                        *guard = Some(mismatch);
                    }
                }
                // Yield so a single-core runner still schedules the
                // reload task.
                std::thread::yield_now();
            }
        })
    };

    // Flip the site's backend and reload. Each iteration changes the
    // answer to "is this host tunnel-backed?", so a torn publication
    // shows up as a routing table that disagrees with the
    // classification.
    const ROUNDS: usize = 100;
    for round in 0..ROUNDS {
        let backend = if round % 2 == 0 {
            "http://127.0.0.1:8080"
        } else {
            "office:http://10.0.0.1:8080"
        };
        {
            let conn = app.db.lock().await;
            db::upsert_site(&conn, &tunnel_site("flip-site", backend)).unwrap();
        }
        app.reload_indexes().await;
    }

    stop.store(true, Ordering::Relaxed);
    sampler.join().unwrap();

    let count = mismatches.load(Ordering::Relaxed);
    assert_eq!(
        count,
        0,
        "the routing table and the tunnel classification disagreed in {count} sample(s): {:?}",
        first_mismatch.lock().unwrap()
    );
}
