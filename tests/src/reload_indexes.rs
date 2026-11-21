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
/// `Arc::ptr_eq` is the deterministic form of the invariant: the set
/// the ALPN callback gets is the very set the published revision
/// holds, so the two views can never come from different revisions.
/// The assertion runs without any concurrent reload, so the two reads
/// cannot straddle a publication.
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
        Arc::ptr_eq(&published.tunnel_domains, &classification.domains),
        "the ALPN callback must read the tunnel classification of the published revision"
    );
    assert!(classification.domains.contains("tun.example.com"));
    assert!(!classification.domains.contains("direct.example.com"));
    assert!(
        Arc::ptr_eq(&published.tunnel_hosts, &classification.hosts),
        "the ALPN callback must read the tun map of the published revision"
    );
    assert_eq!(
        classification
            .hosts
            .get("tun.example.com")
            .map(String::as_str),
        Some("office"),
        "the classification must name the tunnel that serves the domain"
    );
}

/// The hosts the sampler compares. They are picked so the two host
/// rules agree by construction: the exact domain of the flipping
/// site, the exact domain of the direct site and one domain of the
/// bulk site. A host under a wildcard would let `lookup_site` (longest
/// suffix first) and `host_matches_set` (any matching wildcard) answer
/// differently, and such a difference is a property of the rules, not
/// of the publication.
const SAMPLED_HOSTS: [&str; 3] = [
    "flip.example.com",
    "direct.example.com",
    "bulk-0.example.net",
];

/// Do the routing table and the tunnel classification disagree about
/// `host`?
///
/// The request path resolves the host to its site and looks at that
/// site's backend; the handshake path classifies the host against the
/// tunnel set. The two must answer the same question the same way.
fn classification_mismatch(
    revision: &Indexes,
    classification: &std::collections::HashSet<String>,
    host: &str,
) -> Option<String> {
    let routes_through_tunnel = pangolin_core::index::lookup_site(revision, host)
        .map(|site| {
            matches!(
                pangolin_core::parse::parse_backend(&site.backend),
                Ok((ref tun, _)) if !tun.is_empty()
            )
        })
        .unwrap_or(false);
    let classified_as_tunnel = pangolin_core::index::host_matches_set(classification, host);
    if routes_through_tunnel == classified_as_tunnel {
        return None;
    }
    Some(format!(
        "{host}: the routing table sends it {} but the classification says tunnel={classified_as_tunnel}",
        if routes_through_tunnel {
            "through a tunnel"
        } else {
            "to a direct backend"
        }
    ))
}

/// A reload publishes the routing table and the tunnel classification
/// together — the two views are never out of step.
///
/// The sampler runs on its own thread and reads both views the way
/// production reads them: the ALPN callback takes the classification
/// (`App::tunnel_classification`), the request path takes the routing
/// revision (`App::indexes`). The reloads flip the site between a
/// tunnel backend and a direct one underneath it, so the answer to
/// "is this host tunnel-backed?" changes while the sampler runs.
///
/// **Why the classification is read twice.** The two reads bracket the
/// routing read. If they return the same set, the classification did
/// not change while the routing table was read, so a disagreement
/// below is a torn publication — not the sampler racing a reload.
/// A sample that straddles a publication is discarded instead of
/// counted.
///
/// Under the old two-write publication the reload published the
/// routing table first and the classification last, with the cert-link
/// rebuild in between; a sample taken in that window resolved the
/// host against the **new** routing table and classified it against
/// the **old** set, and the two disagreed.
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
    const BULK_DOMAINS: usize = 50;
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
    let discarded = Arc::new(AtomicUsize::new(0));
    // How often the sampler saw the classification change. It proves
    // the sampler really ran across publications instead of passing
    // because it never saw one.
    let observed = Arc::new(AtomicUsize::new(0));
    let first_mismatch: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let sampler = {
        let app = app.clone();
        let stop = stop.clone();
        let mismatches = mismatches.clone();
        let discarded = discarded.clone();
        let observed = observed.clone();
        let first_mismatch = first_mismatch.clone();
        std::thread::spawn(move || {
            let mut previous: Option<pangolin_core::TunnelClassification> = None;
            while !stop.load(Ordering::Relaxed) {
                // The ALPN view, read the way the handshake reads it.
                let before = app.tunnel_classification();
                // The routing view, read the way the request path
                // reads it.
                let revision = app.indexes.read().clone();
                // The ALPN view again — it brackets the routing read.
                let after = app.tunnel_classification();

                let changed = match &previous {
                    Some(prev) => !Arc::ptr_eq(&prev.domains, &before.domains),
                    None => true,
                };
                if changed {
                    observed.fetch_add(1, Ordering::Relaxed);
                }
                previous = Some(before.clone());

                if !Arc::ptr_eq(&before.domains, &after.domains) {
                    // A publication landed between the two reads: the
                    // sample spans two revisions and says nothing.
                    discarded.fetch_add(1, Ordering::Relaxed);
                } else {
                    for host in SAMPLED_HOSTS {
                        if let Some(mismatch) =
                            classification_mismatch(&revision, &before.domains, host)
                        {
                            mismatches.fetch_add(1, Ordering::Relaxed);
                            let mut guard = first_mismatch.lock().unwrap();
                            if guard.is_none() {
                                *guard = Some(mismatch);
                            }
                        }
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

    assert!(
        observed.load(Ordering::Relaxed) > 1,
        "the sampler must have run across publications, but it saw the classification change only {} time(s)",
        observed.load(Ordering::Relaxed)
    );

    let count = mismatches.load(Ordering::Relaxed);
    assert_eq!(
        count,
        0,
        "the routing table and the tunnel classification disagreed in {count} sample(s) \
         ({} sample(s) discarded as straddling a publication): {:?}",
        discarded.load(Ordering::Relaxed),
        first_mismatch.lock().unwrap()
    );
}
