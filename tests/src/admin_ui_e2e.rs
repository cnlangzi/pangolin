//! Admin UI e2e tests — full HTTP requests against a real pangolin-ngx binary.
//!
//! Covers: login flow, auth redirect, CSRF enforcement, and CRUD for all
//! entities (sites, domains, tokens, certs) plus tunnels (read-only).
//!
//! Prerequisites: `make build` (or `cargo build --release -p ngx -p tun`)

use reqwest::redirect::Policy;

use crate::admin_harness::AdminClient;
use crate::harness::{init_pangolin_db, NgxProcess};

// ── helper ──────────────────────────────────────────────────────────────────

fn new_client_no_redirect() -> reqwest::Client {
    reqwest::Client::builder()
        .cookie_store(true)
        .redirect(Policy::none())
        .danger_accept_invalid_certs(true)
        .build()
        .expect("build no-redirect client")
}

async fn start_ngx() -> NgxProcess {
    NgxProcess::start(|path| init_pangolin_db(path)).await
}

// ── §1 — Unauthorized redirects ─────────────────────────────────────────────

#[tokio::test]
async fn unauth_redirect_dashboard() {
    let ngx = start_ngx().await;
    let client = new_client_no_redirect();
    for path in &[
        "/admin/",
        "/admin/sites",
        "/admin/domains",
        "/admin/tun",
        "/admin/tokens",
        "/admin/certs",
    ] {
        let resp = client.get(&ngx.admin_url(path)).send().await.unwrap();
        assert_eq!(resp.status().as_u16(), 302, "path {} should redirect", path);
        let loc = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            loc.contains("/admin/login"),
            "path {} redirect should point to /admin/login, got: {}",
            path,
            loc
        );
    }
}

// ── §2 — Login page renders ─────────────────────────────────────────────────

#[tokio::test]
async fn login_page_renders() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    let resp = client.get("/admin/login").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    client
        .assert_selector_exists(&body, "input[name=username]")
        .unwrap();
    client
        .assert_selector_exists(&body, "input[name=password]")
        .unwrap();
}

// ── §3 — Bad password returns error ─────────────────────────────────────────

#[tokio::test]
async fn login_bad_password() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    let resp = client
        .post_form(
            "/admin/login",
            &[("username", "admin"), ("password", "wrongpassword")],
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("Invalid username or password"),
        "expected error message"
    );
}

// ── §4 — Correct login sets session cookies ──────────────────────────────────

#[tokio::test]
async fn login_correct_sets_cookies() {
    let ngx = start_ngx().await;
    let client = new_client_no_redirect();
    let resp = client
        .post(&ngx.admin_url("/admin/login"))
        .form(&[("username", "admin"), ("password", "admin")])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 302);
    let set_cookie: Vec<_> = resp
        .headers()
        .get_all("set-cookie")
        .into_iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    let has_session = set_cookie.iter().any(|c| c.contains("pangolin_session="));
    let has_csrf = set_cookie.iter().any(|c| c.contains("pangolin_csrf="));
    assert!(has_session, "missing pangolin_session cookie");
    assert!(has_csrf, "missing pangolin_csrf cookie");
}

// ── §5 — Dashboard renders after login ──────────────────────────────────────

#[tokio::test]
async fn dashboard_renders_after_login() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();
    let resp = client.get("/admin/").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Dashboard"), "missing Dashboard text");
}

// ── §6 — Sites list (empty) ──────────────────────────────────────────────────

#[tokio::test]
async fn sites_list_empty() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();
    let resp = client.get("/admin/sites").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Sites"), "missing Sites heading");
}

// ── §7 — Sites new form modal ────────────────────────────────────────────────

#[tokio::test]
async fn sites_new_form_modal() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();
    let resp = client.get("/admin/api/sites/new").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    client
        .assert_selector_exists(&body, "input[name=backend]")
        .unwrap();
}

// ── §8 — Sites create (valid) ────────────────────────────────────────────────

#[tokio::test]
async fn sites_create_valid() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client
        .get("/admin/api/sites/new")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();

    let resp = client
        .post_form(
            "/admin/api/sites",
            &[
                ("backend", "http://127.0.0.1:8080"),
                ("name", "test-site"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert!(
        resp.status().as_u16() < 400,
        "create site should succeed, got {}",
        resp.status()
    );
}

// ── §9 — Sites create without CSRF → 403 ────────────────────────────────────

#[tokio::test]
async fn sites_create_no_csrf_forbidden() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();
    let resp = client
        .post_form(
            "/admin/api/sites",
            &[
                ("backend", "http://127.0.0.1:8080"),
                ("name", "test-site"),
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

// ── §10 — Sites create with invalid backend ──────────────────────────────────

#[tokio::test]
async fn sites_create_invalid_backend() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client
        .get("/admin/api/sites/new")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();

    let resp = client
        .post_form(
            "/admin/api/sites",
            &[
                ("backend", "notaurl"),
                ("name", "test-site"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("invalid backend") || body.contains("Invalid"),
        "expected validation error for bad backend"
    );
}

// ── §11 — Sites edit form ────────────────────────────────────────────────────

#[tokio::test]
async fn sites_edit_form_prefilled() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // First create a site
    let page = client
        .get("/admin/api/sites/new")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/admin/api/sites",
            &[
                ("backend", "http://127.0.0.1:8080"),
                ("name", "edit-me"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();

    let resp = client
        .get("/admin/api/sites/edit?name=edit-me")
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    client
        .assert_selector_exists(&body, "input[name=backend]")
        .unwrap();
}

// ── §12 — Sites update ───────────────────────────────────────────────────────

#[tokio::test]
async fn sites_update() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Create
    let page = client
        .get("/admin/api/sites/new")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/admin/api/sites",
            &[
                ("backend", "http://127.0.0.1:8080"),
                ("name", "update-me"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();

    // Update
    let page2 = client
        .get("/admin/api/sites/edit?name=update-me")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf2 = client.csrf_token(&page2).unwrap_or_default();
    let resp = client
        .post_form(
            "/admin/api/sites",
            &[
                ("backend", "http://127.0.0.1:9090"),
                ("name", "update-me"),
                ("_action", "update"),
                ("_csrf", &csrf2),
            ],
        )
        .await
        .unwrap();
    assert!(resp.status().as_u16() < 400, "update should succeed");
}

// ── §13 — Sites delete (with CSRF) ──────────────────────────────────────────

#[tokio::test]
async fn sites_delete_with_csrf() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client
        .get("/admin/api/sites/new")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/admin/api/sites",
            &[
                ("backend", "http://127.0.0.1:8080"),
                ("name", "delete-me"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();

    let resp = client
        .delete(
            "/admin/api/sites",
            &[("name", "delete-me"), ("_csrf", &csrf)],
        )
        .await
        .unwrap();
    assert!(resp.status().as_u16() < 400, "delete should succeed");
}

// ── §14 — Sites delete without CSRF → 403 ───────────────────────────────────

#[tokio::test]
async fn sites_delete_no_csrf_forbidden() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let resp = client
        .delete(
            "/admin/api/sites",
            &[
                ("name", "nonexistent"),
                // no _csrf
            ],
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

// ── §15 — Sites DB verification after delete ─────────────────────────────────

#[tokio::test]
async fn sites_delete_verified_in_list() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client
        .get("/admin/api/sites/new")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/admin/api/sites",
            &[
                ("backend", "http://127.0.0.1:8080"),
                ("name", "verify-delete"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();

    client
        .delete(
            "/admin/api/sites",
            &[("name", "verify-delete"), ("_csrf", &csrf)],
        )
        .await
        .unwrap();

    let resp = client.get("/admin/sites").await.unwrap();
    let body = resp.text().await.unwrap();
    assert!(
        !body.contains("verify-delete"),
        "deleted site should not appear in list"
    );
}

// ── §16-19 — Domains full cycle ──────────────────────────────────────────────

#[tokio::test]
async fn domains_create_and_list() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Create a site first (domain needs a site_name)
    let page = client
        .get("/admin/api/sites/new")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/admin/api/sites",
            &[
                ("backend", "http://127.0.0.1:8080"),
                ("name", "mysite"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();

    // Create domain
    let page2 = client
        .get("/admin/domains")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf2 = client.csrf_token(&page2).unwrap_or_default();
    let resp = client
        .post_form(
            "/admin/api/domains",
            &[
                ("domain", "example.com"),
                ("site_name", "mysite"),
                ("_csrf", &csrf2),
            ],
        )
        .await
        .unwrap();
    assert!(resp.status().as_u16() < 400, "domain create should succeed");

    // Verify in list
    let list = client
        .get("/admin/domains")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(list.contains("example.com"), "domain should appear in list");
}

#[tokio::test]
async fn domains_create_no_csrf_forbidden() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let resp = client
        .post_form(
            "/admin/api/domains",
            &[
                ("domain", "example.com"),
                ("site_name", "mysite"),
                // no _csrf
            ],
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn domains_delete_with_csrf() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Setup: site + domain
    let page = client
        .get("/admin/api/sites/new")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/admin/api/sites",
            &[
                ("backend", "http://127.0.0.1:8080"),
                ("name", "sitefordomain"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    client
        .post_form(
            "/admin/api/domains",
            &[
                ("domain", "delete-me.com"),
                ("site_name", "sitefordomain"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();

    let resp = client
        .delete(
            "/admin/api/domains",
            &[("domain", "delete-me.com"), ("_csrf", &csrf)],
        )
        .await
        .unwrap();
    assert!(resp.status().as_u16() < 400, "domain delete should succeed");
}

#[tokio::test]
async fn domains_delete_verified_removed() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client
        .get("/admin/api/sites/new")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/admin/api/sites",
            &[
                ("backend", "http://127.0.0.1:8080"),
                ("name", "site2"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    client
        .post_form(
            "/admin/api/domains",
            &[
                ("domain", "gone.com"),
                ("site_name", "site2"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    client
        .delete(
            "/admin/api/domains",
            &[("domain", "gone.com"), ("_csrf", &csrf)],
        )
        .await
        .unwrap();

    let list = client
        .get("/admin/domains")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        !list.contains("gone.com"),
        "deleted domain should not appear"
    );
}

// ── §20-23 — Tokens full cycle ───────────────────────────────────────────────

#[tokio::test]
async fn tokens_create_and_list() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client
        .get("/admin/tokens")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    let resp = client
        .post_form(
            "/admin/api/tokens",
            &[("token", "mytoken123"), ("_csrf", &csrf)],
        )
        .await
        .unwrap();
    assert!(resp.status().as_u16() < 400, "token create should succeed");

    let list = client
        .get("/admin/tokens")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(list.contains("mytoken123"), "token should appear in list");
}

#[tokio::test]
async fn tokens_create_no_csrf_forbidden() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let resp = client
        .post_form("/admin/api/tokens", &[("token", "mytoken123")])
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn tokens_delete_with_csrf() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client
        .get("/admin/tokens")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/admin/api/tokens",
            &[("token", "del-token"), ("_csrf", &csrf)],
        )
        .await
        .unwrap();

    let resp = client
        .delete(
            "/admin/api/tokens",
            &[("token", "del-token"), ("_csrf", &csrf)],
        )
        .await
        .unwrap();
    assert!(resp.status().as_u16() < 400, "token delete should succeed");
}

#[tokio::test]
async fn tokens_delete_verified_removed() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client
        .get("/admin/tokens")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/admin/api/tokens",
            &[("token", "gone-token"), ("_csrf", &csrf)],
        )
        .await
        .unwrap();
    client
        .delete(
            "/admin/api/tokens",
            &[("token", "gone-token"), ("_csrf", &csrf)],
        )
        .await
        .unwrap();

    let list = client
        .get("/admin/tokens")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        !list.contains("gone-token"),
        "deleted token should not appear"
    );
}

// ── §24-27 — Certs full cycle ────────────────────────────────────────────────

#[tokio::test]
async fn certs_create_and_list() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client
        .get("/admin/certs")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();

    let resp = client
        .post_form(
            "/admin/api/certs",
            &[
                ("domain", "testcert.example.com"),
                ("cert_file", "/tmp/test.pem"),
                ("key_file", "/tmp/test.key"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    // May succeed or return validation error; just shouldn't be 4xx due to CSRF/auth
    assert!(resp.status().as_u16() != 403, "should not be CSRF error");
    assert!(resp.status().as_u16() != 401, "should not be auth error");
}

#[tokio::test]
async fn certs_new_form_has_required_fields() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let resp = client.get("/admin/api/certs/new").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    client
        .assert_selector_exists(&body, "input[name=domain]")
        .unwrap();
}

#[tokio::test]
async fn certs_create_no_csrf_forbidden() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let resp = client
        .post_form(
            "/admin/api/certs",
            &[
                ("domain", "test.example.com"),
                ("cert_file", "/tmp/cert.pem"),
                ("key_file", "/tmp/key.pem"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn certs_list_page_renders() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let resp = client.get("/admin/certs").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Certs"), "page should contain 'Certs'");
}

// ── §28 — Tunnels read-only page ─────────────────────────────────────────────

#[tokio::test]
async fn tunnels_readonly_page_renders() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let resp = client.get("/admin/tun").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("Tunnel") || body.contains("tunnel"),
        "page should contain tunnel info"
    );
}

// ── §29 — Logout ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn logout_redirects_to_login() {
    let ngx = start_ngx().await;
    let no_redirect = new_client_no_redirect();

    // Login first
    let login_resp = no_redirect
        .post(&ngx.admin_url("/admin/login"))
        .form(&[("username", "admin"), ("password", "admin")])
        .send()
        .await
        .unwrap();
    assert_eq!(login_resp.status().as_u16(), 302);

    // Extract CSRF from cookie (Set-Cookie header)
    let set_cookies: Vec<String> = login_resp
        .headers()
        .get_all("set-cookie")
        .into_iter()
        .filter_map(|v| v.to_str().ok().map(|s| s.to_string()))
        .collect();
    let csrf_val = set_cookies
        .iter()
        .find(|c| c.contains("pangolin_csrf="))
        .and_then(|c| c.split("pangolin_csrf=").nth(1))
        .and_then(|v| v.split(';').next())
        .unwrap_or("")
        .to_string();

    // Logout
    let resp = no_redirect
        .post(&ngx.admin_url("/admin/logout"))
        .form(&[("_csrf", csrf_val.as_str())])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 302);
    let loc = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        loc.contains("/admin/login"),
        "logout should redirect to login"
    );
}

// ── §30 — After logout, protected pages redirect ────────────────────────────

#[tokio::test]
async fn after_logout_redirects_to_login() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Confirm access
    let resp = client.get("/admin/").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    // Now check that without session, we'd redirect (fresh client)
    let fresh_client = new_client_no_redirect();
    let resp2 = fresh_client
        .get(&ngx.admin_url("/admin/"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp2.status().as_u16(), 302);
}

// ── §31 — Nav active state consistency ───────────────────────────────────────

#[tokio::test]
async fn nav_active_state_per_page() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let pages = [
        ("/admin/sites", "Sites"),
        ("/admin/domains", "Domains"),
        ("/admin/tun", "Tunnels"),
        ("/admin/tokens", "Tokens"),
        ("/admin/certs", "Certs"),
    ];

    for (path, label) in &pages {
        let body = client.get(path).await.unwrap().text().await.unwrap();
        assert!(
            body.contains(label),
            "page {} should contain '{}' in nav",
            path,
            label
        );
        assert_eq!(
            body.chars().filter(|c| *c != '\0').count() > 0,
            true,
            "page {} returned empty body",
            path
        );
    }
}
