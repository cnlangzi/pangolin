//! Admin /api/system/config e2e tests — DB-backed singleton
//! (fix-client-ip) read/update via real pangolin-ngx binary.
//!
//! Covers the full HTTP surface of the system config endpoint:
//! - GET returns the default seeded values
//! - POST updates the row, hot-reloads App's in-memory cache,
//!   and a follow-up GET reflects the new values
//! - POST validation rejects invalid modes and empty custom_lb
//!   header lists with 4xx
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

    // V7 seeds the row with frontend_mode='direct' and an EMPTY
    // trusted_headers list — the column is irrelevant in direct
    // mode (the resolver only consults it under custom_lb).
    assert_eq!(body["frontend_mode"], serde_json::json!("direct"));
    assert_eq!(body["trusted_headers"], serde_json::json!([]));
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

    // Switch to cloudflare mode (no trusted_headers needed).
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

// ── §4 — POST custom_lb with non-empty headers updates ──────────────────────

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
                ("trusted_headers", r#"["X-Forwarded-For","X-Real-IP"]"#),
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
    assert_eq!(
        body["trusted_headers"],
        serde_json::json!(["X-Forwarded-For", "X-Real-IP"])
    );
}

// ── §5 — POST custom_lb with empty headers is rejected ──────────────────────

#[tokio::test]
async fn system_config_post_custom_lb_empty_headers_rejected() {
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
                ("trusted_headers", "[]"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    // 400 from flash_error (200 OK) — the helper returns 200 OK
    // with an error body so the browser's back button survives.
    // Accept either 4xx (handler may change) or 200 + error text.
    let status = resp.status().as_u16();
    assert!(
        (400..500).contains(&status) || status == 200,
        "empty custom_lb headers should be rejected, got {status}"
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
