//! Admin /api/system/config e2e tests — DB-backed singleton
//! (fix-client-ip) read/update via real pangolin-ngx binary.
//!
//! Covers the full HTTP surface of the system config endpoint:
//! - GET returns the default seeded values
//! - POST updates the row, hot-reloads App's in-memory cache,
//!   and a follow-up GET reflects the new values
//! - POST validation rejects invalid modes and invalid first-level
//!   header names with 4xx
//! - POST pgl_headers_enabled=false is persisted
//! - CSRF is enforced globally; POST without a valid token is 403
//!
//! Prerequisite: `make build` (or `cargo build --release -p ngx -p tun`).

use crate::admin_harness::AdminClient;
use crate::harness::{NgxProcess, init_pangolin_db};

async fn start_ngx() -> NgxProcess {
    NgxProcess::start(init_pangolin_db).await
}

// ── §1 — Unauthenticated GET redirects to /login ────────────────────────────

#[tokio::test]
async fn system_config_unauth_redirects_to_login() {
    let ngx = start_ngx().await;
    let client = AdminClient::build_http_client();
    let resp = client
        .get(&format!("{}/api/system/config", ngx.admin_url("")))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "unauthenticated GET should redirect"
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

// ── §2 — GET returns the seeded defaults ────────────────────────────────────

#[tokio::test]
async fn system_config_get_returns_defaults() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let resp = client.get("/api/system/config").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();

    // V7 seeds the row with frontend_mode='direct'. The gateway
    // header knobs keep their documented defaults: the `P-*`
    // headers are enabled, the three first-level header keys are
    // unset (they are irrelevant in direct mode).
    assert_eq!(body["frontend_mode"], serde_json::json!("direct"));
    assert_eq!(body["pgl_headers_enabled"], serde_json::json!(true));
    // V8 seeds `client_ip_header` from the first entry of the old
    // `trusted_headers` list — `X-Real-IP` when the list was empty
    // (the pre-migration default for `custom_lb` mode).
    assert_eq!(body["client_ip_header"], serde_json::json!("X-Real-IP"));
    assert_eq!(body["client_country_header"], serde_json::json!(null));
    assert_eq!(body["client_region_header"], serde_json::json!(null));
    // updated_at is RFC-3339.
    let s = body["updated_at"].as_str().expect("updated_at string");
    assert!(
        chrono::DateTime::parse_from_rfc3339(s).is_ok(),
        "updated_at must be RFC-3339, got: {s}"
    );
}

// ── §3 — POST updates the row, GET reflects it ──────────────────────────────

#[tokio::test]
async fn system_config_post_updates_and_reloads() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Fetch CSRF from the dashboard (any authenticated page works).
    let csrf_page = client.get("/").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&csrf_page).expect("csrf token on /");

    // Switch to cloudflare mode (the built-in CF-* headers are used).
    let resp = client
        .post_form(
            "/api/system/config",
            &[("frontend_mode", "cloudflare"), ("_csrf", &csrf)],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "successful POST should redirect to /"
    );

    // GET reflects the change.
    let body: serde_json::Value = client
        .get("/api/system/config")
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["frontend_mode"], serde_json::json!("cloudflare"));
}

// ── §4 — POST custom_lb with a configured header key updates ─────────────────
#[tokio::test]
async fn system_config_post_custom_lb_with_headers() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let csrf = client
        .csrf_token(&client.get("/").await.unwrap().text().await.unwrap())
        .unwrap();

    let resp = client
        .post_form(
            "/api/system/config",
            &[
                ("frontend_mode", "custom_lb"),
                ("client_ip_header", "X-Client-IP"),
                ("client_country_header", "X-Client-Country"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 302);

    let body: serde_json::Value = client
        .get("/api/system/config")
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["frontend_mode"], serde_json::json!("custom_lb"));
    assert_eq!(body["client_ip_header"], serde_json::json!("X-Client-IP"));
    assert_eq!(
        body["client_country_header"],
        serde_json::json!("X-Client-Country")
    );
    // An unset key stays unset.
    assert_eq!(body["client_region_header"], serde_json::json!(null));
}

// ── §5 — POST custom_lb without a header key is accepted ────────────────────
#[tokio::test]
async fn system_config_post_custom_lb_without_headers_accepted() {
    // An unconfigured key is valid: the matching value is simply
    // empty. The old `trusted_headers` list was mandatory in this
    // mode; the per-value keys are not (issue #115).
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let csrf = client
        .csrf_token(&client.get("/").await.unwrap().text().await.unwrap())
        .unwrap();

    let resp = client
        .post_form(
            "/api/system/config",
            &[("frontend_mode", "custom_lb"), ("_csrf", &csrf)],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "custom_lb without a header key must be accepted"
    );

    let body: serde_json::Value = client
        .get("/api/system/config")
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["frontend_mode"], serde_json::json!("custom_lb"));
    assert_eq!(body["client_ip_header"], serde_json::json!(null));
}

// ── §5b — POST with an invalid header name is rejected ──────────────────────
#[tokio::test]
async fn system_config_post_invalid_header_name_rejected() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let csrf = client
        .csrf_token(&client.get("/").await.unwrap().text().await.unwrap())
        .unwrap();

    let resp = client
        .post_form(
            "/api/system/config",
            &[
                ("frontend_mode", "custom_lb"),
                ("client_ip_header", "bad header"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    // 400 from flash_error (200 OK) — the helper returns 200 OK
    // with an error body so the browser's back button survives.
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
    let body: serde_json::Value = client
        .get("/api/system/config")
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["frontend_mode"], serde_json::json!("direct"));
    // The row is untouched — still the V8-seeded default.
    assert_eq!(body["client_ip_header"], serde_json::json!("X-Real-IP"));
}

// ── §5c — POST pgl_headers_enabled=false is persisted ───────────────────────
#[tokio::test]
async fn system_config_post_disables_gateway_headers() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let csrf = client
        .csrf_token(&client.get("/").await.unwrap().text().await.unwrap())
        .unwrap();

    let resp = client
        .post_form(
            "/api/system/config",
            &[
                ("frontend_mode", "direct"),
                ("pgl_headers_enabled", "false"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 302);

    let body: serde_json::Value = client
        .get("/api/system/config")
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["pgl_headers_enabled"], serde_json::json!(false));
}

// ── §6 — POST invalid mode is rejected ──────────────────────────────────────

#[tokio::test]
async fn system_config_post_invalid_mode_rejected() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let csrf = client
        .csrf_token(&client.get("/").await.unwrap().text().await.unwrap())
        .unwrap();

    let resp = client
        .post_form(
            "/api/system/config",
            &[("frontend_mode", "not-a-real-mode"), ("_csrf", &csrf)],
        )
        .await
        .unwrap();
    let status = resp.status().as_u16();
    assert!(
        (400..500).contains(&status) || status == 200,
        "invalid mode should be rejected, got {status}"
    );

    // GET still shows defaults.
    let body: serde_json::Value = client
        .get("/api/system/config")
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["frontend_mode"], serde_json::json!("direct"));
}

// ── §7 — POST without CSRF is forbidden ─────────────────────────────────────

#[tokio::test]
async fn system_config_post_no_csrf_forbidden() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let resp = client
        .post_form(
            "/api/system/config",
            &[
                ("frontend_mode", "direct"),
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
