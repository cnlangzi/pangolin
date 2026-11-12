//! Admin /settings e2e tests — HTML form for the system_config
//! singleton (fix-client-ip).
//!
//! Covers:
//! - GET /settings requires auth (302 → /login)
//! - GET /settings renders the form with current values + CSRF
//! - POST /settings with valid input → 302 to /settings; row updated
//! - POST /settings with custom_lb + no header key → accepted
//! - POST /settings with an invalid header name → re-render with
//!   inline error; row unchanged
//! - POST /settings with bad mode → re-render with inline error
//! - POST without CSRF → 403
//!
//! Prerequisite: `make build` (or `cargo build --release -p ngx -p tun`).

use crate::admin_harness::AdminClient;
use crate::harness::{NgxProcess, init_pangolin_db};

async fn start_ngx() -> NgxProcess {
    NgxProcess::start(init_pangolin_db).await
}

// ── §1 — Unauthenticated GET redirects to /login ────────────────────────────

#[tokio::test]
async fn settings_unauth_redirects_to_login() {
    let ngx = start_ngx().await;
    let client = AdminClient::build_http_client();
    let resp = client
        .get(&ngx.admin_url("/settings"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "unauthenticated GET /settings should redirect"
    );
    let loc = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        loc.contains("/login"),
        "redirect should point to /login, got {loc}"
    );
}

// ── §2 — GET renders form with current values ──────────────────────────────

#[tokio::test]
async fn settings_get_renders_with_defaults() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let resp = client.get("/settings").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();

    // Form targets /settings (POST).
    assert!(
        body.contains(r#"action="/settings""#),
        "form action missing"
    );
    // CSRF token present.
    assert!(body.contains(r#"name="_csrf""#), "CSRF input missing");
    // Direct mode is the seeded default — radio button pre-selected.
    assert!(
        body.contains(r#"value="direct" checked"#),
        "direct mode should be pre-selected; \
         the seeded default is direct"
    );
    // Cloudflare + custom_lb radio buttons present but unchecked.
    assert!(body.contains(r#"value="cloudflare""#));
    assert!(body.contains(r#"value="custom_lb""#));
    // The gateway-header switch and the three first-level header
    // inputs are always visible (issue #115).
    assert!(
        body.contains(r#"name="pgl_headers_enabled""#),
        "pgl_headers_enabled input missing"
    );
    for field in [
        "client_ip_header",
        "client_country_header",
        "client_region_header",
    ] {
        assert!(
            body.contains(&format!(r#"name="{field}""#)),
            "{field} input missing"
        );
    }
    // The old trusted_headers textarea is gone.
    assert!(
        !body.contains(r#"name="trusted_headers""#),
        "trusted_headers textarea must be removed"
    );
}

// ── §3 — POST with cloudflare succeeds and round-trips ─────────────────────

#[tokio::test]
async fn settings_post_cloudflare_round_trip() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
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
        "successful POST should redirect to /settings"
    );

    // GET reflects the change.
    let body = client.get("/settings").await.unwrap().text().await.unwrap();
    assert!(
        body.contains(r#"value="cloudflare" checked"#),
        "cloudflare should be pre-selected after POST; got body: {}",
        body
    );
}

// ── §4 — POST custom_lb without a header key is accepted ────────────────────
#[tokio::test]
async fn settings_post_custom_lb_without_headers_accepted() {
    // An unconfigured first-level header key is valid — the matching
    // value is simply empty (issue #115). The old `trusted_headers`
    // list was mandatory in this mode.
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let csrf = client
        .csrf_token(&client.get("/settings").await.unwrap().text().await.unwrap())
        .unwrap();

    let resp = client
        .post_form("/settings", &[("provider", "custom_lb"), ("_csrf", &csrf)])
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "custom_lb without a header key must be accepted"
    );

    // GET reflects the change.
    let body = client.get("/settings").await.unwrap().text().await.unwrap();
    assert!(
        body.contains(r#"value="custom_lb" checked"#),
        "custom_lb should be pre-selected after POST; got body: {}",
        body
    );
}

// ── §4b — POST with an invalid header name is rejected ──────────────────────
#[tokio::test]
async fn settings_post_invalid_header_name_rejected() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let csrf = client
        .csrf_token(&client.get("/settings").await.unwrap().text().await.unwrap())
        .unwrap();

    let resp = client
        .post_form(
            "/settings",
            &[
                ("provider", "custom_lb"),
                ("client_ip_header", "bad header"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    // Validation error re-renders the form. flash_error would be 200
    // OK with an error fragment; we just check the error is surfaced
    // and names the offending field.
    let status = resp.status().as_u16();
    assert!(
        (400..500).contains(&status) || status == 200,
        "an invalid header name should be rejected, got {status}"
    );
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("client_ip_header"),
        "the error must name the offending field, got: {body}"
    );

    // GET still shows the prior mode (defaults) — the update was not applied.
    let body = client.get("/settings").await.unwrap().text().await.unwrap();
    assert!(
        body.contains(r#"value="direct" checked"#),
        "direct mode should remain after rejected POST; \
         body shows otherwise: {}",
        body
    );
}

// ── §5 — POST invalid mode is rejected ─────────────────────────────────────

#[tokio::test]
async fn settings_post_invalid_mode_rejected() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let csrf = client
        .csrf_token(&client.get("/settings").await.unwrap().text().await.unwrap())
        .unwrap();

    let resp = client
        .post_form(
            "/settings",
            &[("provider", "not-a-real-mode"), ("_csrf", &csrf)],
        )
        .await
        .unwrap();
    let status = resp.status().as_u16();
    assert!(
        (400..500).contains(&status) || status == 200,
        "invalid mode should be rejected, got {status}"
    );

    // Defaults preserved.
    let body = client.get("/settings").await.unwrap().text().await.unwrap();
    assert!(
        body.contains(r#"value="direct" checked"#),
        "direct mode should remain after invalid-mode POST"
    );
}

// ── §6 — POST without CSRF is forbidden ────────────────────────────────────

#[tokio::test]
async fn settings_post_no_csrf_forbidden() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let resp = client
        .post_form(
            "/settings",
            &[
                ("provider", "direct"),
                // no _csrf
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "missing CSRF should be forbidden"
    );
}

// ── §7 — The IP reputation settings (issue #141) ───────────────────────────
//
// The reputation configuration lives in the database, not in
// `ngx.yml`, and is edited from its own form on the settings page.

#[tokio::test]
async fn settings_page_renders_the_reputation_form() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let body = client.get("/settings").await.unwrap().text().await.unwrap();
    assert!(
        body.contains(r#"action="/settings/badbots""#),
        "the reputation form must be present"
    );
    for field in [
        "badbots_enabled",
        "badbots_evidence_policy",
        "badbots_feed_refresh_interval_secs",
        "badbots_abuseipdb_api_key",
        "badbots_crowdsec_api_key",
    ] {
        assert!(
            body.contains(&format!(r#"name="{field}""#)),
            "{field} input missing"
        );
    }
    // The check is on by default (the feature needs no configuration)
    // — but the e2e harness turns it off, so the page renders it
    // unchecked. The seeded default itself is pinned by the
    // `badbots_config_seeded_after_migrate` unit test.
    assert!(
        body.contains(r#"name="badbots_enabled" value="true""#),
        "the reputation switch must be present"
    );
    assert!(
        !body.contains(r#"name="badbots_enabled" value="true" checked"#),
        "the e2e harness turns the reputation check off"
    );
}

#[tokio::test]
async fn settings_post_reputation_saves_the_row_without_echoing_the_key() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let csrf = client
        .csrf_token(&client.get("/settings").await.unwrap().text().await.unwrap())
        .expect("csrf on /settings");

    let resp = client
        .post_form(
            "/settings/badbots",
            &[
                ("_csrf", &csrf),
                ("badbots_enabled", "true"),
                ("badbots_evidence_policy", "strict"),
                ("badbots_feed_refresh_interval_secs", "600"),
                ("badbots_abuseipdb_api_key", "secret-key"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "a successful save redirects back to the settings page"
    );

    // The row carries the new settings.
    let conn = rusqlite::Connection::open(ngx.db_path()).expect("open db");
    let enabled: i64 = conn
        .query_row("SELECT enabled FROM badbots_config WHERE id = 1", [], |r| {
            r.get(0)
        })
        .expect("the row exists");
    assert_eq!(enabled, 1, "the check must stay on");
    let policy: String = conn
        .query_row(
            "SELECT evidence_policy FROM badbots_config WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .expect("the row exists");
    assert_eq!(policy, "strict", "the policy must be stored");
    let key: Option<String> = conn
        .query_row(
            "SELECT abuseipdb_api_key FROM badbots_config WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .expect("the row exists");
    assert_eq!(key.as_deref(), Some("secret-key"), "the key must be stored");

    // The key is never rendered back into the page.
    let body = client.get("/settings").await.unwrap().text().await.unwrap();
    assert!(
        !body.contains("secret-key"),
        "the stored key must never be rendered back into the page"
    );
    assert!(
        body.contains("configured"),
        "the page must say that a key is configured"
    );
}

#[tokio::test]
async fn settings_post_reputation_rejects_a_bad_policy_inline() {
    // A failure re-renders the settings page with the message inline
    // — not a standalone error page — so the operator sees it where
    // they were.
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let csrf = client
        .csrf_token(&client.get("/settings").await.unwrap().text().await.unwrap())
        .expect("csrf on /settings");

    let resp = client
        .post_form(
            "/settings/badbots",
            &[
                ("_csrf", &csrf),
                ("badbots_enabled", "true"),
                ("badbots_evidence_policy", "bogus"),
                ("badbots_feed_refresh_interval_secs", "300"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "the page is re-rendered");
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("bogus"),
        "the error must name the offending value: {body}"
    );
    assert!(
        body.contains(r#"action="/settings/badbots""#),
        "the reputation form must still be on the page: {body}"
    );

    // The row is unchanged — a rejected form never writes.
    let conn = rusqlite::Connection::open(ngx.db_path()).expect("open db");
    let policy: String = conn
        .query_row(
            "SELECT evidence_policy FROM badbots_config WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .expect("the row exists");
    assert_eq!(policy, "lenient", "a rejected form must not write");
}
