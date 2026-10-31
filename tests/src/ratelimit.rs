//! Per-site rate limiting integration tests (issue #117).
//!
//! Covers three layers:
//!   - the DB layer (`site_rate_limits` + `ip_rules`),
//!   - the index build (the policy and the effective rule set of every
//!     site are assembled together),
//!   - the runtime limiter (the request / page dimensions, the IP
//!     rules and the known-bot exemption).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

use chrono::Utc;
use rusqlite::Connection;
use tempfile::TempDir;

use pangolin_core::db;
use pangolin_core::ratelimit::{IpRule, LimitDecision, RateLimitPolicy, RuleKind};
use pangolin_core::types::{Domain, HostMode, Site};
use pangolin_core::{App, CertManager};

fn ip(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
    format!("{a}.{b}.{c}.{d}").parse().unwrap()
}

fn policy(req_limit: u32, page_limit: u32) -> RateLimitPolicy {
    RateLimitPolicy {
        enabled: true,
        req_limit,
        req_window_secs: 60,
        page_limit,
        page_window_secs: 3600,
        ip_capacity: 256,
        throttle_capacity: 256,
        throttle_secs: 600,
        custom_429_body: None,
    }
}

fn site(name: &str) -> Site {
    Site {
        name: name.into(),
        backend: "http://127.0.0.1:8080".into(),
        enabled: true,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        host_mode: HostMode::Passthrough,
        host_custom: None,
        domain_count: 0,
    }
}

fn domain(domain: &str, site_name: &str) -> Domain {
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
fn site_rate_limit_policy_round_trip() {
    let (_dir, conn) = temp_conn();
    assert!(db::list_site_rate_limits(&conn).unwrap().is_empty());

    let p = policy(120, 900);
    db::upsert_site_rate_limit(&conn, "site-a", &p).unwrap();

    let back = db::get_site_rate_limit(&conn, "site-a").unwrap();
    assert_eq!(back, Some(p.clone()));

    let all = db::list_site_rate_limits(&conn).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all["site-a"], p);

    // Upserting again overwrites — the row is keyed by the site name.
    let mut p2 = p.clone();
    p2.req_limit = 7;
    db::upsert_site_rate_limit(&conn, "site-a", &p2).unwrap();
    assert_eq!(db::list_site_rate_limits(&conn).unwrap().len(), 1);
    assert_eq!(
        db::get_site_rate_limit(&conn, "site-a")
            .unwrap()
            .unwrap()
            .req_limit,
        7
    );

    assert!(db::delete_site_rate_limit(&conn, "site-a").unwrap());
    assert!(db::get_site_rate_limit(&conn, "site-a").unwrap().is_none());
}

#[test]
fn the_custom_429_page_round_trips() {
    let (_dir, conn) = temp_conn();
    let mut p = policy(10, 0);
    p.custom_429_body = Some("<h1>Slow down</h1>".to_string());
    db::upsert_site_rate_limit(&conn, "site-a", &p).unwrap();

    let back = db::get_site_rate_limit(&conn, "site-a").unwrap().unwrap();
    assert_eq!(back.custom_429_body.as_deref(), Some("<h1>Slow down</h1>"));

    // An empty body means "use the built-in default page".
    let mut p2 = policy(10, 0);
    p2.custom_429_body = Some("   ".to_string());
    db::upsert_site_rate_limit(&conn, "site-a", &p2).unwrap();
    let back = db::get_site_rate_limit(&conn, "site-a").unwrap().unwrap();
    assert_eq!(back.custom_429_body, None);
}

#[test]
fn ip_rules_round_trip_and_delete() {
    let (_dir, conn) = temp_conn();
    assert!(db::list_ip_rules(&conn).unwrap().is_empty());

    let rule = IpRule {
        id: "global|blacklist|203.0.113.0/24".into(),
        scope: "global".into(),
        kind: RuleKind::Blacklist,
        cidr: "203.0.113.0/24".into(),
        note: Some("scraper range".into()),
        created_at: Utc::now(),
    };
    db::upsert_ip_rule(&conn, &rule).unwrap();

    let rules = db::list_ip_rules(&conn).unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].scope, "global");
    assert_eq!(rules[0].kind, RuleKind::Blacklist);
    assert_eq!(rules[0].cidr, "203.0.113.0/24");

    // The natural key makes a re-add an upsert, not a duplicate.
    db::upsert_ip_rule(&conn, &rule).unwrap();
    assert_eq!(db::list_ip_rules(&conn).unwrap().len(), 1);

    assert!(db::delete_ip_rule(&conn, &rule.id).unwrap());
    assert!(db::list_ip_rules(&conn).unwrap().is_empty());
}

// ── Index build ────────────────────────────────────────────────────

#[tokio::test]
async fn index_merges_global_and_site_scoped_rules() {
    let dir = TempDir::new().unwrap();
    let app = make_test_app(&dir);

    {
        let db = app.db.lock().await;
        db::upsert_site(&db, &site("a")).unwrap();
        db::upsert_site(&db, &site("b")).unwrap();
        db::upsert_domain(&db, &domain("a.test", "a")).unwrap();
        db::upsert_domain(&db, &domain("b.test", "b")).unwrap();
        db::upsert_site_rate_limit(&db, "a", &policy(10, 0)).unwrap();
        let now = Utc::now();
        db::upsert_ip_rule(
            &db,
            &IpRule {
                id: "global|blacklist|203.0.113.0/24".into(),
                scope: "global".into(),
                kind: RuleKind::Blacklist,
                cidr: "203.0.113.0/24".into(),
                note: None,
                created_at: now,
            },
        )
        .unwrap();
        db::upsert_ip_rule(
            &db,
            &IpRule {
                id: "a|whitelist|198.51.100.7".into(),
                scope: "a".into(),
                kind: RuleKind::Whitelist,
                cidr: "198.51.100.7".into(),
                note: None,
                created_at: now,
            },
        )
        .unwrap();
    }
    app.reload_indexes().await;

    let indexes = app.indexes.read().await;
    let a = indexes.limits.get("a").expect("site a has a limit config");
    let b = indexes.limits.get("b").expect("site b has a limit config");

    // The global rule applies everywhere.
    assert_eq!(a.rule_match(ip(203, 0, 113, 5)), Some(RuleKind::Blacklist));
    assert_eq!(b.rule_match(ip(203, 0, 113, 5)), Some(RuleKind::Blacklist));
    // The site-scoped rule only applies to its site.
    assert_eq!(a.rule_match(ip(198, 51, 100, 7)), Some(RuleKind::Whitelist));
    assert_eq!(b.rule_match(ip(198, 51, 100, 7)), None);
    // The policy of "a" is loaded; "b" has no row → limits off.
    assert!(a.policy.enabled);
    assert_eq!(a.policy.req_limit, 10);
    assert!(!b.policy.enabled);
}

// ── Runtime limiter ────────────────────────────────────────────────

#[tokio::test]
async fn limiter_throttles_over_the_request_limit() {
    let dir = TempDir::new().unwrap();
    let app = make_test_app(&dir);
    {
        let db = app.db.lock().await;
        db::upsert_site(&db, &site("a")).unwrap();
        db::upsert_site_rate_limit(&db, "a", &policy(2, 0)).unwrap();
    }
    app.reload_indexes().await;

    let cfg = app.indexes.read().await.limits.get("a").cloned().unwrap();
    let client = ip(198, 51, 100, 10);

    assert_eq!(
        app.rate_limiter
            .check("a", &cfg, Some(client), false, Some("/")),
        LimitDecision::Allow
    );
    assert_eq!(
        app.rate_limiter
            .check("a", &cfg, Some(client), false, Some("/")),
        LimitDecision::Allow
    );
    match app
        .rate_limiter
        .check("a", &cfg, Some(client), false, Some("/"))
    {
        LimitDecision::Throttled { retry_after } => {
            assert!(
                (1..=60).contains(&retry_after),
                "Retry-After must be within the counting window, got {retry_after}"
            );
        }
        other => panic!("expected throttled, got {other:?}"),
    }
}

#[tokio::test]
async fn limiter_counts_distinct_pages() {
    let dir = TempDir::new().unwrap();
    let app = make_test_app(&dir);
    {
        let db = app.db.lock().await;
        db::upsert_site(&db, &site("a")).unwrap();
        db::upsert_site_rate_limit(&db, "a", &policy(0, 5)).unwrap();
    }
    app.reload_indexes().await;

    let cfg = app.indexes.read().await.limits.get("a").cloned().unwrap();
    let client = ip(198, 51, 100, 11);

    // Five distinct URLs stay inside the limit.
    for i in 0..5 {
        let key = format!("/p{i}");
        assert_eq!(
            app.rate_limiter
                .check("a", &cfg, Some(client), false, Some(&key)),
            LimitDecision::Allow,
            "page {i} must be within the limit"
        );
    }
    // The same URL again does not grow the distinct-page count.
    for _ in 0..20 {
        assert_eq!(
            app.rate_limiter
                .check("a", &cfg, Some(client), false, Some("/p0")),
            LimitDecision::Allow
        );
    }
    // A crawler that walks twenty distinct URLs is rejected.
    let mut decision = LimitDecision::Allow;
    for i in 5..20 {
        let key = format!("/p{i}");
        decision = app
            .rate_limiter
            .check("a", &cfg, Some(client), false, Some(&key));
    }
    assert!(
        matches!(decision, LimitDecision::Throttled { .. }),
        "twenty distinct pages must trip the page limit, got {decision:?}"
    );
}

#[tokio::test]
async fn limiter_exempts_a_verified_known_bot() {
    let dir = TempDir::new().unwrap();
    let app = make_test_app(&dir);
    {
        let db = app.db.lock().await;
        db::upsert_site(&db, &site("a")).unwrap();
        db::upsert_site_rate_limit(&db, "a", &policy(1, 1)).unwrap();
    }
    app.reload_indexes().await;

    // Seed the knownbots verifier with the Googlebot range so the
    // verdict can be produced without a network refresh.
    let verifier = app.bot_verifier.as_ref().expect("bot verifier");
    verifier.seed_prefix("googlebot", "66.249.64.0/19").unwrap();

    let cfg = app.indexes.read().await.limits.get("a").cloned().unwrap();
    let bot = ip(66, 249, 66, 1);
    let ua = "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)";

    // The same request from an ordinary client is rejected…
    let mut decision = LimitDecision::Allow;
    for _ in 0..5 {
        decision = app
            .rate_limiter
            .check("a", &cfg, Some(ip(198, 51, 100, 12)), false, Some("/"));
    }
    assert!(
        matches!(decision, LimitDecision::Throttled { .. }),
        "an ordinary client must be throttled, got {decision:?}"
    );

    // …while the verified bot is served normally.
    let verified = pangolin_core::verify_bot(verifier, ua, bot).is_some();
    assert!(verified, "the seeded Googlebot CIDR must verify");
    for _ in 0..5 {
        assert_eq!(
            app.rate_limiter
                .check("a", &cfg, Some(bot), verified, Some("/")),
            LimitDecision::Allow
        );
    }
}

#[tokio::test]
async fn limiter_blocks_a_blacklisted_ip() {
    let dir = TempDir::new().unwrap();
    let app = make_test_app(&dir);
    {
        let db = app.db.lock().await;
        db::upsert_site(&db, &site("a")).unwrap();
        db::upsert_site_rate_limit(&db, "a", &policy(0, 0)).unwrap();
        let now = Utc::now();
        db::upsert_ip_rule(
            &db,
            &IpRule {
                id: "global|blacklist|203.0.113.0/24".into(),
                scope: "global".into(),
                kind: RuleKind::Blacklist,
                cidr: "203.0.113.0/24".into(),
                note: None,
                created_at: now,
            },
        )
        .unwrap();
    }
    app.reload_indexes().await;

    let cfg = app.indexes.read().await.limits.get("a").cloned().unwrap();
    assert_eq!(
        app.rate_limiter
            .check("a", &cfg, Some(ip(203, 0, 113, 9)), false, Some("/")),
        LimitDecision::Blocked
    );
    // The rule applies even though the site's limits are off.
    assert_eq!(
        app.rate_limiter
            .check("a", &cfg, Some(ip(198, 51, 100, 13)), false, Some("/")),
        LimitDecision::Allow
    );
}

#[tokio::test]
async fn reload_indexes_picks_up_a_changed_policy() {
    let dir = TempDir::new().unwrap();
    let app = make_test_app(&dir);
    {
        let db = app.db.lock().await;
        db::upsert_site(&db, &site("a")).unwrap();
        db::upsert_site_rate_limit(&db, "a", &policy(1, 0)).unwrap();
    }
    app.reload_indexes().await;

    let cfg = app.indexes.read().await.limits.get("a").cloned().unwrap();
    let client = ip(198, 51, 100, 14);
    assert_eq!(
        app.rate_limiter
            .check("a", &cfg, Some(client), false, Some("/")),
        LimitDecision::Allow
    );
    assert!(matches!(
        app.rate_limiter
            .check("a", &cfg, Some(client), false, Some("/")),
        LimitDecision::Throttled { .. }
    ));

    // Raise the limit — the next request is served again.
    {
        let db = app.db.lock().await;
        db::upsert_site_rate_limit(&db, "a", &policy(100, 0)).unwrap();
    }
    app.reload_indexes().await;
    let cfg = app.indexes.read().await.limits.get("a").cloned().unwrap();
    assert_eq!(cfg.policy.req_limit, 100);
    assert_eq!(
        app.rate_limiter
            .check("a", &cfg, Some(client), false, Some("/")),
        LimitDecision::Allow
    );
}

#[tokio::test]
async fn deleting_a_site_stops_its_policy() {
    let dir = TempDir::new().unwrap();
    let app = make_test_app(&dir);
    {
        let db = app.db.lock().await;
        db::upsert_site(&db, &site("a")).unwrap();
        db::upsert_domain(&db, &domain("a.test", "a")).unwrap();
        db::upsert_site_rate_limit(&db, "a", &policy(1, 0)).unwrap();
    }
    app.reload_indexes().await;
    assert!(
        app.indexes
            .read()
            .await
            .limits
            .get("a")
            .expect("site a is limited")
            .policy
            .enabled
    );

    {
        let db = app.db.lock().await;
        db::delete_site(&db, "a").unwrap();
        db::delete_site_rate_limit(&db, "a").unwrap();
    }
    app.reload_indexes().await;

    let indexes = app.indexes.read().await;
    assert!(indexes.limits.get("a").is_none());
    assert!(
        pangolin_core::index::lookup_site(&indexes, "a.test").is_none(),
        "the domain of a deleted site must not resolve"
    );
}

#[tokio::test]
async fn deleting_a_site_removes_its_policy_and_scoped_rules() {
    let dir = TempDir::new().unwrap();
    let app = make_test_app(&dir);
    let now = Utc::now();
    {
        let db = app.db.lock().await;
        db::upsert_site(&db, &site("a")).unwrap();
        db::upsert_site_rate_limit(&db, "a", &policy(10, 0)).unwrap();
        db::upsert_ip_rule(
            &db,
            &IpRule {
                id: "a|blacklist|203.0.113.0/24".into(),
                scope: "a".into(),
                kind: RuleKind::Blacklist,
                cidr: "203.0.113.0/24".into(),
                note: None,
                created_at: now,
            },
        )
        .unwrap();
        db::upsert_ip_rule(
            &db,
            &IpRule {
                id: "global|blacklist|198.51.100.0/24".into(),
                scope: "global".into(),
                kind: RuleKind::Blacklist,
                cidr: "198.51.100.0/24".into(),
                note: None,
                created_at: now,
            },
        )
        .unwrap();
    }

    {
        let db = app.db.lock().await;
        assert!(db::delete_site(&db, "a").unwrap());
    }

    let db = app.db.lock().await;
    // The site's policy and its scoped rules are gone — re-creating a
    // site with the same name must not revive them.
    assert!(db::get_site_rate_limit(&db, "a").unwrap().is_none());
    let rules = db::list_ip_rules(&db).unwrap();
    assert_eq!(
        rules.len(),
        1,
        "only the global rule must survive: {rules:?}"
    );
    assert_eq!(rules[0].scope, "global");
}

#[tokio::test]
async fn a_policy_row_without_a_site_is_ignored() {
    let dir = TempDir::new().unwrap();
    let app = make_test_app(&dir);
    {
        let db = app.db.lock().await;
        db::upsert_site_rate_limit(&db, "ghost", &policy(1, 0)).unwrap();
    }
    app.reload_indexes().await;
    assert!(
        app.indexes.read().await.limits.get("ghost").is_none(),
        "a policy of a site that does not exist must be ignored"
    );
}

#[tokio::test]
async fn unknown_client_ip_is_neither_counted_nor_limited() {
    let dir = TempDir::new().unwrap();
    let app = make_test_app(&dir);
    {
        let db = app.db.lock().await;
        db::upsert_site(&db, &site("a")).unwrap();
        db::upsert_site_rate_limit(&db, "a", &policy(1, 0)).unwrap();
    }
    app.reload_indexes().await;

    let cfg = app.indexes.read().await.limits.get("a").cloned().unwrap();
    for _ in 0..10 {
        assert_eq!(
            app.rate_limiter.check("a", &cfg, None, false, Some("/")),
            LimitDecision::Allow
        );
    }
    let snap = app.ratelimit_snapshot();
    assert_eq!(snap.totals.counted, 0);
}

#[tokio::test]
async fn two_sites_have_independent_counters() {
    let dir = TempDir::new().unwrap();
    let app = make_test_app(&dir);
    {
        let db = app.db.lock().await;
        db::upsert_site(&db, &site("a")).unwrap();
        db::upsert_site(&db, &site("b")).unwrap();
        db::upsert_site_rate_limit(&db, "a", &policy(1, 0)).unwrap();
        db::upsert_site_rate_limit(&db, "b", &policy(1, 0)).unwrap();
    }
    app.reload_indexes().await;

    let indexes = app.indexes.read().await;
    let cfg_a = indexes.limits.get("a").cloned().unwrap();
    let cfg_b = indexes.limits.get("b").cloned().unwrap();
    drop(indexes);

    let client = ip(198, 51, 100, 15);
    assert_eq!(
        app.rate_limiter
            .check("a", &cfg_a, Some(client), false, Some("/")),
        LimitDecision::Allow
    );
    // Site "b" has its own counter.
    assert_eq!(
        app.rate_limiter
            .check("b", &cfg_b, Some(client), false, Some("/")),
        LimitDecision::Allow
    );
    assert!(matches!(
        app.rate_limiter
            .check("a", &cfg_a, Some(client), false, Some("/")),
        LimitDecision::Throttled { .. }
    ));
}

#[test]
fn policy_map_is_built_for_every_site() {
    // A site without a policy row still gets an entry — with the
    // default policy (limits off).
    let names = vec!["a".to_string(), "b".to_string()];
    let policies: HashMap<String, RateLimitPolicy> = HashMap::new();
    let configs = pangolin_core::ratelimit::build_limit_configs(&names, &policies, &[]);
    assert_eq!(configs.len(), 2);
    assert!(!configs["a"].policy.enabled);
    assert!(!configs["b"].policy.enabled);
}
