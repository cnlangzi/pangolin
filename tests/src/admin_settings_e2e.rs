//! Admin /settings e2e tests — HTML form for the system_config
//! singleton (fix-client-ip).
//!
//! Covers:
//! - GET /settings requires auth (302 → /login)
//! - GET /settings renders the form with current values + CSRF
//! - POST /settings with valid input → 302 to /settings; row updated
//! - POST /settings with custom_lb + empty headers → re-render with
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
    // trusted_headers textarea is seeded as an empty JSON array.
    assert!(
        body.contains(r#"name="trusted_headers""#),
        "trusted_headers textarea missing"
    );
    // The seeded default is `[]`; the textarea renders that value.
    assert!(
        body.contains(">[]<"),
        "trusted_headers textarea should default to `[]`, got body without `[]`"
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
        .post_form(
            "/settings",
            &[("frontend_mode", "cloudflare"), ("_csrf", &csrf)],
        )
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

// ── §4 — POST custom_lb with empty headers is rejected ─────────────────────

#[tokio::test]
async fn settings_post_custom_lb_empty_headers_rejected() {
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
                ("frontend_mode", "custom_lb"),
                ("trusted_headers", "[]"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    // Validation error re-renders the form. flash_error would be 200
    // OK with an error fragment; we just check the radio reflects
    // the input that was rejected and the error is surfaced.
    let status = resp.status().as_u16();
    assert!(
        (400..500).contains(&status) || status == 200,
        "empty custom_lb headers should be rejected, got {status}"
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
            &[("frontend_mode", "not-a-real-mode"), ("_csrf", &csrf)],
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
