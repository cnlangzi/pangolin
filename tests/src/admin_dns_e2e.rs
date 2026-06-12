//! DNS provider e2e tests — full HTTP requests against a real pangolin-ngx binary.
//!
//! Covers the four DNS pages (/dns, /dns/new, /dns/{name}/edit, /dns/{name}/delete)
//! plus the /dns/test connection probe.
//!
//! These tests close the gap that let the /dns/new SVG/pattern/hidden-required/
//! test-endpoint-403 regressions ship undetected: prior admin_ui_e2e.rs had no
//! DNS coverage at all, so the broken markup passed review.
//!
//! Prerequisites: `make build` (or `cargo build --release -p ngx -p tun`)

use crate::admin_harness::AdminClient;
use crate::harness::{init_pangolin_db, NgxProcess};
use scraper::{Html, Selector};

fn new_client_no_redirect() -> reqwest::Client {
    AdminClient::build_http_client()
}

async fn start_ngx() -> NgxProcess {
    NgxProcess::start(|path| init_pangolin_db(path)).await
}

// ── §1 — Unauthenticated requests redirect to /login ─────────────────────────

#[tokio::test]
async fn dns_unauth_redirects_to_login() {
    let ngx = start_ngx().await;
    let client = new_client_no_redirect();
    for path in &["/dns", "/dns/new"] {
        let resp = client.get(&ngx.admin_url(path)).send().await.unwrap();
        assert_eq!(resp.status().as_u16(), 302, "{} should redirect", path);
        let loc = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            loc.contains("/login"),
            "{} redirect should point to /login, got {}",
            path,
            loc
        );
    }
}

// ── §2 — Empty DNS list page renders ─────────────────────────────────────────

#[tokio::test]
async fn dns_list_empty_renders() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();
    let resp = client.get("/dns").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("DNS Providers"), "list page missing heading");
    // Empty-state copy must be present
    assert!(
        body.contains("No DNS providers configured"),
        "empty-state copy missing"
    );
    // Must link to the create page
    assert!(body.contains("href=\"/dns/new\""));
}

// ── §3 — New DNS page renders all three kind panels + Cloudflare default ────

#[tokio::test]
async fn dns_new_page_renders_all_panels() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();
    let resp = client.get("/dns/new").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();

    // The page is a full HTML document (not a modal)
    assert!(body.contains("<html"), "new DNS should be a full page");

    // The three kind panels exist (data-kind-panel="…")
    assert!(body.contains(r#"data-kind-panel="cloudflare""#));
    assert!(body.contains(r#"data-kind-panel="aliyun""#));
    assert!(body.contains(r#"data-kind-panel="tencent""#));

    // Form has CSRF and the action
    assert!(body.contains(r#"action="/dns/new""#));
    assert!(body.contains(r#"name="_csrf""#));

    // Name input has the /v-safe pattern (escaped hyphen)
    assert!(
        body.contains(r#"pattern="[a-z0-9_\-]+""#),
        "name input pattern must use escaped hyphen to be valid in /v regex mode"
    );

    // Cloudflare is the default kind
    let doc = Html::parse_document(&body);
    let sel = Selector::parse(r#"input[name="kind"][value="cloudflare"]"#).unwrap();
    let checked = doc
        .select(&sel)
        .next()
        .and_then(|el| el.value().attr("checked"))
        .is_some();
    assert!(checked, "cloudflare should be the default kind");
}

// ── §4 — New DNS page does NOT contain the broken SVG path string ────────────

#[tokio::test]
async fn dns_new_page_no_broken_svg_path() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();
    let body = client.get("/dns/new").await.unwrap().text().await.unwrap();
    // The original bug was an SVG path with 7 numbers for a c-command (which
    // takes 6): "c-.866-1.5-3-1.5-3-3.898 0L…". The fix uses the canonical
    // Heroicons 6-number form: "c-.866-1.5-3.032-1.5-3.898 0L…".
    assert!(
        !body.contains("-3-1.5-3-3.898"),
        "broken 7-number cubic-bezier variant leaked into the page; \
         Chrome rejects it with 'Expected number' SVG errors."
    );
    assert!(
        body.contains("-3.032-1.5-3.898"),
        "expected the canonical Heroicons path with -3.032 control point"
    );
}

// ── §5 — Create Cloudflare provider → 302 redirect ──────────────────────────

#[tokio::test]
async fn dns_create_cloudflare_redirects() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client.get("/dns/new").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();

    let resp = client
        .post_form(
            "/dns/new",
            &[
                ("name", "main-cf"),
                ("kind", "cloudflare"),
                ("api_token", "fake-test-token-12345"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "create cloudflare DNS should redirect, got {}",
        resp.status()
    );
    let loc = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        loc.contains("/dns"),
        "should redirect to /dns, got: {}",
        loc
    );
}

// ── §6 — Create without CSRF → 403 ───────────────────────────────────────────

#[tokio::test]
async fn dns_create_no_csrf_forbidden() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let resp = client
        .post_form(
            "/dns/new",
            &[
                ("name", "no-csrf"),
                ("kind", "cloudflare"),
                ("api_token", "tok"),
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

// ── §7 — Invalid name (uppercase) re-renders form with error ────────────────

#[tokio::test]
async fn dns_create_invalid_name_shows_error() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client.get("/dns/new").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();

    let resp = client
        .post_form(
            "/dns/new",
            &[
                ("name", "Bad-Name-With-Caps"),
                ("kind", "cloudflare"),
                ("api_token", "tok"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "validation error should re-render the form (200), not redirect"
    );
    // Structural markers only: the form should re-render with the name
    // input still present. We don't pin the exact error wording, and we
    // don't currently require the server to echo the submitted name back
    // (a future enhancement, mirroring `sites_create_preserves_form_values_on_error`).
    let body = resp.text().await.unwrap();
    client
        .assert_selector_exists(&body, r#"input[name="name"]"#)
        .expect("name input should still be present after validation error");
}

// ── §8 — Test connection endpoint exists and rejects bad input ─────────────
//
// This is the exact endpoint the browser hits via fetch(). It MUST live at
// /dns/test (not /admin/dns/test) and reply with a JSON envelope. The
// "missing kind" case simultaneously proves (a) the route exists and accepts
// CSRF, (b) static validation rejects bad input, and (c) the response is
// JSON {ok:false}. Consolidated from two earlier tests that each checked
// part of this — one combined test is easier to maintain.

#[tokio::test]
async fn dns_test_endpoint_exists_and_handles_bad_input() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client.get("/dns/new").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();

    // No `kind` field → static validation rejects → JSON 400 envelope.
    let resp = client
        .post_form("/dns/test", &[("_csrf", &csrf)])
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "missing kind should produce a 400 JSON response, got {}",
        resp.status()
    );
    // Must NOT be 403 (CSRF body was accepted) or 404 (route is /dns/test,
    // not the legacy /admin/dns/test path).
    assert_ne!(resp.status().as_u16(), 403);
    assert_ne!(resp.status().as_u16(), 404);

    // JSON shape: parse and assert the `ok` field is false. We don't pin
    // the exact error wording.
    let body = resp.text().await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("test response must be JSON, got {}: {}", e, body));
    assert_eq!(
        v.get("ok").and_then(|x| x.as_bool()),
        Some(false),
        "ok should be false, got body: {}",
        body
    );
}

// ── §9 — Test connection succeeds with valid Cloudflare config ──────────────

#[tokio::test]
async fn dns_test_cloudflare_valid_succeeds() {
    // We can't actually call Cloudflare from CI, but the server's
    // static_validate_config should pass: a non-empty api_token is all the
    // static check requires. The remote API call would fail at runtime, but
    // for this test we only assert that the endpoint reaches the handler
    // (returns 200 with ok:true OR 400 with a network-related error).
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client.get("/dns/new").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();

    let resp = client
        .post_form(
            "/dns/test",
            &[
                ("name", "main-cf"),
                ("kind", "cloudflare"),
                ("api_token", "fake-but-non-empty"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    // The server's static check passes (non-empty token); the actual API
    // call will fail and return {ok:false, error:"..."} with status 400.
    // Either outcome is fine — what matters is that the endpoint is wired up.
    assert!(
        resp.status().as_u16() == 200 || resp.status().as_u16() == 400,
        "test endpoint should respond with 200/400, got {}",
        resp.status()
    );
    let body = resp.text().await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("test response must be JSON, got {}: {}", e, body));
    assert!(
        v.get("ok").and_then(|x| x.as_bool()).is_some(),
        "response must have an `ok` boolean field, got: {}",
        body
    );
}

// ── §10 — Test connection fails when Cloudflare api_token is empty ──────────

#[tokio::test]
async fn dns_test_cloudflare_empty_token_rejected() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client.get("/dns/new").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();

    let resp = client
        .post_form(
            "/dns/test",
            &[
                ("name", "main-cf"),
                ("kind", "cloudflare"),
                // no api_token
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    // Status-code + JSON shape only; exact error message wording is
    // implementation detail and may change.
    assert_eq!(
        resp.status().as_u16(),
        400,
        "missing api_token should produce a 400 JSON response, got {}",
        resp.status()
    );
    let body = resp.text().await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("test response must be JSON, got {}: {}", e, body));
    assert_eq!(
        v.get("ok").and_then(|x| x.as_bool()),
        Some(false),
        "ok should be false on validation failure, got: {}",
        body
    );
}

// ── §11 — Create Aliyun provider succeeds ───────────────────────────────────

#[tokio::test]
async fn dns_create_aliyun_redirects() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client.get("/dns/new").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();

    let resp = client
        .post_form(
            "/dns/new",
            &[
                ("name", "aliyun-prod"),
                ("kind", "aliyun"),
                ("access_key_id", "LTAI-test-12345"),
                ("access_key_secret", "secret-value"),
                ("region", "cn-hangzhou"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "create aliyun DNS should redirect, got {}",
        resp.status()
    );

    // Verify it appears in the list
    let list = client.get("/dns").await.unwrap().text().await.unwrap();
    assert!(list.contains("aliyun-prod"));
    assert!(list.contains("aliyun"), "kind label should be in the list");
}

// ── §12 — Edit page is prefilled ────────────────────────────────────────────

#[tokio::test]
async fn dns_edit_page_prefilled() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Create one first
    let page = client.get("/dns/new").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/dns/new",
            &[
                ("name", "edit-me"),
                ("kind", "cloudflare"),
                ("api_token", "tok"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();

    // Open edit page
    let resp = client.get("/dns/edit-me/edit").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Edit DNS Provider"));
    // Name is shown readonly
    let doc = Html::parse_document(&body);
    let sel = Selector::parse(r#"input[name="name"]"#).unwrap();
    let name_input = doc.select(&sel).next().expect("name input present");
    assert_eq!(name_input.value().attr("value"), Some("edit-me"));
    assert!(
        name_input.value().attr("readonly").is_some(),
        "name should be readonly on edit"
    );
}

// ── §13 — Delete redirects → /dns and provider is gone ──────────────────────

#[tokio::test]
async fn dns_delete_removes_provider() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Create
    let page = client.get("/dns/new").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/dns/new",
            &[
                ("name", "delete-me"),
                ("kind", "cloudflare"),
                ("api_token", "tok"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();

    // Delete
    let list_page = client.get("/dns").await.unwrap().text().await.unwrap();
    let list_csrf = client.csrf_token(&list_page).unwrap_or_default();
    let resp = client
        .post_form("/dns/delete-me/delete", &[("_csrf", &list_csrf)])
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 302);

    // Verify gone
    let list_after = client.get("/dns").await.unwrap().text().await.unwrap();
    assert!(
        !list_after.contains("delete-me"),
        "deleted provider should not appear in list"
    );
}

// ── §14 — Delete without CSRF → 403 ─────────────────────────────────────────

#[tokio::test]
async fn dns_delete_no_csrf_forbidden() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();
    // Send a non-empty body (just `name`) so the pingora body-read doesn't
    // stall on empty POSTs; the missing _csrf is what the CSRF middleware
    // is supposed to flag.
    let resp = client
        .post_form("/dns/anything/delete", &[("name", "anything")])
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "missing _csrf should be forbidden"
    );
}

// ── §15 — Page renders without broken SVG or invalid pattern regex ──────────

#[tokio::test]
async fn dns_list_page_no_broken_svg() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();
    let body = client.get("/dns").await.unwrap().text().await.unwrap();
    // The list page only shows a "+" add button, not an exclamation SVG, but
    // assert the regression check for completeness.
    assert!(
        !body.contains("-3-1.5-3-3.898"),
        "broken SVG path leaked into /dns list page"
    );
}

// ── §16 — /dns/new has CSRF token embedded (catches submit-side CSRF bugs) ──

#[tokio::test]
async fn dns_new_form_embeds_csrf_token() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();
    let body = client.get("/dns/new").await.unwrap().text().await.unwrap();
    let csrf = client
        .csrf_token(&body)
        .expect("csrf token should be extractable");
    assert!(
        !csrf.is_empty() && csrf.chars().all(|c| c.is_ascii_alphanumeric()),
        "CSRF should be a non-empty hex string, got: {:?}",
        csrf
    );
}
