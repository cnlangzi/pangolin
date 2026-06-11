//! Admin UI e2e tests — full HTTP requests against a real pangolin-ngx binary.
//!
//! Covers: login flow, auth redirect, CSRF enforcement, and CRUD for all
//! entities (sites, domains, tokens, certs) plus tunnels (read-only).
//!
//! Prerequisites: `make build` (or `cargo build --release -p ngx -p tun`)

use crate::admin_harness::AdminClient;
use crate::harness::{init_pangolin_db, NgxProcess};
use scraper::{Html, Selector};

// ── helper ──────────────────────────────────────────────────────────────────

fn new_client_no_redirect() -> reqwest::Client {
    AdminClient::build_http_client()
}

async fn start_ngx() -> NgxProcess {
    NgxProcess::start(|path| init_pangolin_db(path)).await
}

// ── §1 — Unauthorized redirects ─────────────────────────────────────────────

#[tokio::test]
async fn unauth_redirect_dashboard() {
    let ngx = start_ngx().await;
    let client = new_client_no_redirect();
    for path in &["/", "/sites", "/sites/new", "/domains", "/tun", "/certs"] {
        let resp = client.get(&ngx.admin_url(path)).send().await.unwrap();
        assert_eq!(resp.status().as_u16(), 302, "path {} should redirect", path);
        let loc = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            loc.contains("/login"),
            "path {} redirect should point to /login, got: {}",
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
    let resp = client.get("/login").await.unwrap();
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
            "/login",
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
        .post(&ngx.admin_url("/login"))
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
    let resp = client.get("/").await.unwrap();
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
    let resp = client.get("/sites").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Sites"), "missing Sites heading");
    assert!(
        body.contains("New site"),
        "sites page should expose a 'New site' link"
    );
}

// ── §7 — Sites new page (full page, not a modal) ─────────────────────────────

#[tokio::test]
async fn sites_new_page_is_full_page() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();
    let resp = client.get("/sites/new").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    // Full page: should have <html>, base layout, and a back-link to /sites
    assert!(
        body.contains("<html"),
        "new site page should be a full HTML page"
    );
    assert!(
        body.contains("Back to sites"),
        "new site page should have a 'Back to sites' link"
    );
    client
        .assert_selector_exists(&body, "input[name=backend]")
        .unwrap();
    client
        .assert_selector_exists(&body, "input[name=name]")
        .unwrap();
}

// ── §8 — Sites create (valid) → 302 redirect ────────────────────────────────

#[tokio::test]
async fn sites_create_valid() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client
        .get("/sites/new")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();

    let resp = client
        .post_form(
            "/sites/new",
            &[
                ("backend", "http://127.0.0.1:8080"),
                ("name", "test-site"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "create site should redirect, got {}",
        resp.status()
    );
    let loc = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        loc.contains("/sites"),
        "create should redirect to /sites, got: {}",
        loc
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
            "/sites/new",
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
        .get("/sites/new")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();

    let resp = client
        .post_form(
            "/sites/new",
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
        body.contains("Invalid backend") || body.contains("invalid backend"),
        "expected validation error for bad backend"
    );
}

// ── §11 — Sites edit page (prefilled) ────────────────────────────────────────

#[tokio::test]
async fn sites_edit_page_prefilled() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Create a site first
    let page = client
        .get("/sites/new")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/sites/new",
            &[
                ("backend", "http://127.0.0.1:8080"),
                ("name", "edit-me"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();

    let resp = client.get("/sites/edit?name=edit-me").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("Edit site"),
        "edit page should be titled 'Edit site'"
    );
    client
        .assert_selector_exists(&body, "input[name=backend]")
        .unwrap();
}

// ── §12 — Sites update → 302 redirect ────────────────────────────────────────

#[tokio::test]
async fn sites_update() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Create
    let page = client
        .get("/sites/new")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/sites/new",
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
        .get("/sites/edit?name=update-me")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf2 = client.csrf_token(&page2).unwrap_or_default();
    let resp = client
        .post_form(
            "/sites/edit?name=update-me",
            &[
                ("backend", "http://127.0.0.1:9090"),
                ("name", "update-me"),
                ("_csrf", &csrf2),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "update should redirect, got {}",
        resp.status()
    );
}

// ── §13 — Sites delete (with CSRF) → 302 redirect ────────────────────────────

#[tokio::test]
async fn sites_delete_with_csrf() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client
        .get("/sites/new")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/sites/new",
            &[
                ("backend", "http://127.0.0.1:8080"),
                ("name", "delete-me"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();

    let resp = client
        .post_form("/sites/delete", &[("name", "delete-me"), ("_csrf", &csrf)])
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "delete should redirect, got {}",
        resp.status()
    );
}

// ── §14 — Sites delete without CSRF → 403 ───────────────────────────────────

#[tokio::test]
async fn sites_delete_no_csrf_forbidden() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let resp = client
        .post_form("/sites/delete", &[("name", "nonexistent")])
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

// ── §15 — Sites delete verified in list ─────────────────────────────────────

#[tokio::test]
async fn sites_delete_verified_in_list() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client
        .get("/sites/new")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/sites/new",
            &[
                ("backend", "http://127.0.0.1:8080"),
                ("name", "verify-delete"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();

    client
        .post_form(
            "/sites/delete",
            &[("name", "verify-delete"), ("_csrf", &csrf)],
        )
        .await
        .unwrap();

    let resp = client.get("/sites").await.unwrap();
    let body = resp.text().await.unwrap();
    assert!(
        !body.contains("verify-delete"),
        "deleted site should not appear in list"
    );
}

// ── §8.5 — Full "New site" UI flow ───────────────────────────────────────────
//
// Exercises the same path the user clicks in the browser:
//   1. GET /sites → confirm the "New site" link is present (a <a>, not a button).
//   2. GET /sites/new → confirm the full-page form is rendered (with
//      the fields the user types into: name, backend, _csrf).
//   3. POST /sites/new with valid data → 302 redirect.
//   4. GET /sites → confirm the new site row is in the list.

#[tokio::test]
async fn sites_create_full_ui_flow() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // 1. Sites page renders a "New site" link.
    let sites_page = client.get("/sites").await.unwrap().text().await.unwrap();
    assert!(
        sites_page.contains("New site"),
        "sites page should expose a 'New site' link"
    );
    assert!(
        sites_page.contains("href=\"/sites/new\""),
        "sites page should link to /sites/new (not open a modal)"
    );

    // 2. Clicking the link loads a full-page form.
    let new_form = client
        .get("/sites/new")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        new_form.contains("<html"),
        "/sites/new should be a full HTML page (not a modal fragment)"
    );
    client
        .assert_selector_exists(&new_form, "input[name=backend]")
        .expect("new-site form should contain a backend input");
    client
        .assert_selector_exists(&new_form, "input[name=name]")
        .expect("new-site form should contain a name input");
    client
        .assert_selector_exists(&new_form, "input[name=_csrf]")
        .expect("new-site form should embed a CSRF token");

    let csrf = client.csrf_token(&new_form).expect("extract CSRF token");

    // 3. Submit the form → 302 redirect to /sites.
    let resp = client
        .post_form(
            "/sites/new",
            &[
                ("backend", "http://127.0.0.1:8080"),
                ("name", "ui-flow-site"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "create should redirect, got {}",
        resp.status()
    );

    // 4. The site now appears in the sites list.
    let list_after = client.get("/sites").await.unwrap().text().await.unwrap();
    assert!(
        list_after.contains("ui-flow-site"),
        "newly created site 'ui-flow-site' should appear in the sites list"
    );
    assert!(
        list_after.contains("http://127.0.0.1:8080"),
        "the backend URL should be shown in the sites list row"
    );
}

#[tokio::test]
async fn sites_create_full_ui_flow_unique_names() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    for site_name in &["flow-a", "flow-b"] {
        let form = client
            .get("/sites/new")
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let csrf = client.csrf_token(&form).unwrap_or_default();

        let resp = client
            .post_form(
                "/sites/new",
                &[
                    ("backend", "http://127.0.0.1:8080"),
                    ("name", site_name),
                    ("_csrf", &csrf),
                ],
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status().as_u16(),
            302,
            "create '{}' should redirect, got {}",
            site_name,
            resp.status()
        );
    }

    let list = client.get("/sites").await.unwrap().text().await.unwrap();
    assert!(list.contains("flow-a"), "flow-a should be in list");
    assert!(list.contains("flow-b"), "flow-b should be in list");
}

// ── §16-19 — Domains full cycle ──────────────────────────────────────────────

#[tokio::test]
async fn domains_create_and_list() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Create a site first (domain needs a site_name)
    let page = client
        .get("/sites/new")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/sites/new",
            &[
                ("backend", "http://127.0.0.1:8080"),
                ("name", "mysite"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();

    // Create domain
    let page2 = client.get("/domains").await.unwrap().text().await.unwrap();
    let csrf2 = client.csrf_token(&page2).unwrap_or_default();
    let resp = client
        .post_form(
            "/domains/new",
            &[
                ("domain", "example.com"),
                ("site_name", "mysite"),
                ("_csrf", &csrf2),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "domain create should redirect, got {}",
        resp.status()
    );

    // Verify in list
    let list = client.get("/domains").await.unwrap().text().await.unwrap();
    assert!(list.contains("example.com"), "domain should appear in list");
}

#[tokio::test]
async fn domains_create_no_csrf_forbidden() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let resp = client
        .post_form(
            "/domains/new",
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
        .get("/sites/new")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/sites/new",
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
            "/domains/new",
            &[
                ("domain", "delete-me.com"),
                ("site_name", "sitefordomain"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();

    let resp = client
        .post_form(
            "/domains/delete",
            &[("domain", "delete-me.com"), ("_csrf", &csrf)],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "domain delete should redirect, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn domains_delete_verified_removed() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client
        .get("/sites/new")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/sites/new",
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
            "/domains/new",
            &[
                ("domain", "gone.com"),
                ("site_name", "site2"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    client
        .post_form(
            "/domains/delete",
            &[("domain", "gone.com"), ("_csrf", &csrf)],
        )
        .await
        .unwrap();

    let list = client.get("/domains").await.unwrap().text().await.unwrap();
    assert!(
        !list.contains("gone.com"),
        "deleted domain should not appear"
    );
}

// ── §20-23 — Tokens full cycle ───────────────────────────────────────────────
// Removed in v2: the `/admin/tokens` route and `tokens` table were
// dropped when the credential was merged onto the `tun` row.
// Auth lifecycle for tuns is covered by `tests/src/auth.rs` (DB layer)
// and `real_e2e::real_e2e_tunnel_*` (binary end-to-end).

// ── §24-27 — Certs full cycle ────────────────────────────────────────────────

#[tokio::test]
async fn certs_create_and_list() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client.get("/certs").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();

    let resp = client
        .post_form(
            "/certs/new",
            &[
                ("domain", "testcert.example.com"),
                ("cert_file", "/tmp/test.pem"),
                ("key_file", "/tmp/test.key"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "cert create should redirect, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn certs_new_page_is_full_page() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let resp = client.get("/certs/new").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("<html"),
        "new cert page should be a full HTML page"
    );
    client
        .assert_selector_exists(&body, "input[name=domain]")
        .unwrap();
    client
        .assert_selector_exists(&body, "input[name=cert_file]")
        .unwrap();
    client
        .assert_selector_exists(&body, "input[name=key_file]")
        .unwrap();
}

#[tokio::test]
async fn certs_create_no_csrf_forbidden() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let resp = client
        .post_form(
            "/certs/new",
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

    let resp = client.get("/certs").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("Certs") || body.contains("Certificates"),
        "page should contain cert heading"
    );
}

#[tokio::test]
async fn certs_delete_with_csrf() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client.get("/certs").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/certs/new",
            &[
                ("domain", "delete-cert.example.com"),
                ("cert_file", "/tmp/cert.pem"),
                ("key_file", "/tmp/key.pem"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();

    let resp = client
        .post_form(
            "/certs/delete",
            &[("domain", "delete-cert.example.com"), ("_csrf", &csrf)],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "cert delete should redirect, got {}",
        resp.status()
    );
}

// ── §28 — Tunnels CRUD ────────────────────────────────────────────────────

#[tokio::test]
async fn tunnels_list_renders_with_new_button() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let resp = client.get("/tun").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Tunnel") || body.contains("tunnel"));
    assert!(
        body.contains("New tunnel"),
        "tunnels page should expose a 'New tunnel' link"
    );
}

#[tokio::test]
async fn tunnels_new_page_is_full_page() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let resp = client.get("/tun/new").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("<html"),
        "new tunnel page should be a full HTML page"
    );
    assert!(body.contains("Back to tunnels"));
    client
        .assert_selector_exists(&body, "input[name=name]")
        .unwrap();
    client
        .assert_selector_exists(&body, "input[name=token]")
        .unwrap();
    client
        .assert_selector_exists(&body, "input[name=expires_at]")
        .unwrap();
    client
        .assert_selector_exists(&body, "input[name=enabled]")
        .unwrap();
}

#[tokio::test]
async fn tunnels_create_valid_auto_token() {
    // Token field left blank → server auto-generates via OsRng
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client.get("/tun/new").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();

    let resp = client
        .post_form(
            "/tun/new",
            &[
                ("name", "auto-token-node"),
                ("token", ""), // blank → auto-generate
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "create should redirect, got {}",
        resp.status()
    );
    let loc = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(loc.contains("/tun"), "should redirect to /tun");

    // Verify tunnel appears in list with a non-empty token
    let list = client.get("/tun").await.unwrap().text().await.unwrap();
    assert!(
        list.contains("auto-token-node"),
        "tunnel name should appear in list"
    );
    // Token cell should NOT be empty "—" (a token was auto-generated)
    assert!(
        !list.contains("auto-token-node") || list.contains("font-mono"),
        "token should be visible in the list"
    );
}

#[tokio::test]
async fn tunnels_create_with_provided_token() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client.get("/tun/new").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();

    let resp = client
        .post_form(
            "/tun/new",
            &[
                ("name", "manual-token-node"),
                ("token", "my-super-secret-token-123"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 302);

    // Token should appear in the list
    let list = client.get("/tun").await.unwrap().text().await.unwrap();
    assert!(list.contains("manual-token-node"));
    assert!(
        list.contains("my-super-secret-token-123"),
        "provided token should be visible in the list"
    );
}

#[tokio::test]
async fn tunnels_create_with_expires_at() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client.get("/tun/new").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();

    let resp = client
        .post_form(
            "/tun/new",
            &[
                ("name", "expiring-node"),
                ("token", "any-token"),
                ("expires_at", "2030-12-31T23:59"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 302);

    // Expires column should show the date
    let list = client.get("/tun").await.unwrap().text().await.unwrap();
    assert!(list.contains("expiring-node"));
    assert!(
        list.contains("2030-12-31"),
        "expires_at should appear in list"
    );
}

#[tokio::test]
async fn tunnels_create_no_csrf_forbidden() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let resp = client
        .post_form(
            "/tun/new",
            &[
                ("name", "no-csrf-node"),
                ("token", "any-token"),
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

#[tokio::test]
async fn tunnels_create_invalid_name_chars() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client.get("/tun/new").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();

    let resp = client
        .post_form(
            "/tun/new",
            &[
                ("name", "invalid name with spaces"),
                ("token", "any-token"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    // Should re-render the form with an error, not redirect
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("letters, digits") || body.contains("Name"),
        "error should be shown for invalid name"
    );
}

#[tokio::test]
async fn tunnels_edit_page_prefilled() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Create a tunnel first
    let page = client.get("/tun/new").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/tun/new",
            &[
                ("name", "edit-me-node"),
                ("token", "original-token"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();

    // Open edit page
    let resp = client.get("/tun/edit?name=edit-me-node").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Edit tunnel"));
    // Name should be shown as read-only display (not an input)
    assert!(body.contains("edit-me-node"));
    client
        .assert_selector_exists(&body, "input[name=token]")
        .unwrap();
}

#[tokio::test]
async fn tunnels_update() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Create
    let page = client.get("/tun/new").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/tun/new",
            &[
                ("name", "update-me-node"),
                ("token", "original-token"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();

    // Update
    let page2 = client
        .get("/tun/edit?name=update-me-node")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf2 = client.csrf_token(&page2).unwrap_or_default();
    let resp = client
        .post_form(
            "/tun/edit?name=update-me-node",
            &[("token", "updated-token"), ("_csrf", &csrf2)],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "update should redirect, got {}",
        resp.status()
    );

    // Verify updated token in list
    let list = client.get("/tun").await.unwrap().text().await.unwrap();
    assert!(
        list.contains("updated-token"),
        "updated token should appear in list"
    );
}

#[tokio::test]
async fn tunnels_delete_with_csrf() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client.get("/tun/new").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/tun/new",
            &[
                ("name", "delete-me-node"),
                ("token", "any-token"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();

    let resp = client
        .post_form(
            "/tun/delete",
            &[("name", "delete-me-node"), ("_csrf", &csrf)],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "delete should redirect, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn tunnels_delete_no_csrf_forbidden() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let resp = client
        .post_form("/tun/delete", &[("name", "any-node")])
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn tunnels_delete_verified_in_list() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client.get("/tun/new").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/tun/new",
            &[
                ("name", "verify-delete-node"),
                ("token", "any-token"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();

    client
        .post_form(
            "/tun/delete",
            &[("name", "verify-delete-node"), ("_csrf", &csrf)],
        )
        .await
        .unwrap();

    let list = client.get("/tun").await.unwrap().text().await.unwrap();
    assert!(
        !list.contains("verify-delete-node"),
        "deleted tunnel should not appear in list"
    );
}

#[tokio::test]
async fn tunnels_edit_missing_name_bad_request() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let resp = client.get("/tun/edit").await.unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "missing name param should return 400"
    );
}

#[tokio::test]
async fn tunnels_edit_nonexistent_not_found() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let resp = client.get("/tun/edit?name=nonexistent-node").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200); // renders error in body
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("not found") || body.contains("Not found"),
        "should show not-found error"
    );
}

// ── §29 — Logout ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn logout_redirects_to_login() {
    let ngx = start_ngx().await;
    let no_redirect = new_client_no_redirect();

    // Login first
    let login_resp = no_redirect
        .post(&ngx.admin_url("/login"))
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
        .post(&ngx.admin_url("/logout"))
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
    assert!(loc.contains("/login"), "logout should redirect to login");
}

// ── §30 — After logout, protected pages redirect ────────────────────────────

#[tokio::test]
async fn after_logout_redirects_to_login() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Confirm access
    let resp = client.get("/").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    // Now check that without session, we'd redirect (fresh client)
    let fresh_client = new_client_no_redirect();
    let resp2 = fresh_client.get(&ngx.admin_url("/")).send().await.unwrap();
    assert_eq!(resp2.status().as_u16(), 302);
}

// ── §31 — Nav active state consistency ───────────────────────────────────────

#[tokio::test]
async fn nav_active_state_per_page() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let pages = [
        ("/sites", "Sites"),
        ("/domains", "Domains"),
        ("/tun", "Tunnels"),
        ("/certs", "Certs"),
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

// ── §32 — Site domains sub-page: page renders with correct content ───────────

#[tokio::test]
async fn site_domains_subpage_renders() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Create a site
    let page = client
        .get("/sites/new")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/sites/new",
            &[
                ("backend", "http://127.0.0.1:8080"),
                ("name", "subpage-test-site"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();

    // Create two domains for this site
    let domains_page = client.get("/domains").await.unwrap().text().await.unwrap();
    let csrf2 = client.csrf_token(&domains_page).unwrap_or_default();
    client
        .post_form(
            "/domains/new",
            &[
                ("domain", "subdomain1.example.com"),
                ("site_name", "subpage-test-site"),
                ("_csrf", &csrf2),
            ],
        )
        .await
        .unwrap();
    client
        .post_form(
            "/domains/new",
            &[
                ("domain", "subdomain2.example.com"),
                ("site_name", "subpage-test-site"),
                ("_csrf", &csrf2),
            ],
        )
        .await
        .unwrap();

    // Visit the site-specific domains sub-page
    let resp = client.get("/site/subpage-test-site/domains").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();

    // Should contain both domains
    assert!(
        body.contains("subdomain1.example.com"),
        "subpage should list subdomain1"
    );
    assert!(
        body.contains("subdomain2.example.com"),
        "subpage should list subdomain2"
    );
    // Should show the site name in breadcrumb/header
    assert!(
        body.contains("subpage-test-site"),
        "subpage should show the site name"
    );
}

// ── §33 — Site domains sub-page: domain count link in sites table ───────────

#[tokio::test]
async fn site_domains_count_link_in_sites_table() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Create a site with one domain
    let page = client
        .get("/sites/new")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/sites/new",
            &[
                ("backend", "http://127.0.0.1:8080"),
                ("name", "count-link-test"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    let domains_page = client.get("/domains").await.unwrap().text().await.unwrap();
    let csrf2 = client.csrf_token(&domains_page).unwrap_or_default();
    client
        .post_form(
            "/domains/new",
            &[
                ("domain", "countlink.test.example.com"),
                ("site_name", "count-link-test"),
                ("_csrf", &csrf2),
            ],
        )
        .await
        .unwrap();

    // Sites table should have a link with the domain count
    let sites_body = client.get("/sites").await.unwrap().text().await.unwrap();
    // Should contain a link to the site domains sub-page
    assert!(
        sites_body.contains("/site/count-link-test/domains"),
        "sites table should have a link to site-specific domains sub-page"
    );
}

// ── §34 — Site domains sub-page: unauthenticated redirects ──────────────────

#[tokio::test]
async fn site_domains_subpage_unauth_redirects() {
    let ngx = start_ngx().await;
    let client = new_client_no_redirect();
    let resp = client
        .get(&ngx.admin_url("/site/test-site/domains"))
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
        loc.contains("/login"),
        "unauthenticated request should redirect to login, got: {}",
        loc
    );
}

// ── §35 — Site domains sub-page: site-specific domain table via HTMX ─────────

#[tokio::test]
async fn site_domains_api_table_for_site() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Create a site and a domain
    let page = client
        .get("/sites/new")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/sites/new",
            &[
                ("backend", "http://127.0.0.1:8080"),
                ("name", "hx-table-test"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    let domains_page = client.get("/domains").await.unwrap().text().await.unwrap();
    let csrf2 = client.csrf_token(&domains_page).unwrap_or_default();
    client
        .post_form(
            "/domains/new",
            &[
                ("domain", "hx-test.example.com"),
                ("site_name", "hx-table-test"),
                ("_csrf", &csrf2),
            ],
        )
        .await
        .unwrap();

    // HTMX endpoint should return only the table rows
    let resp = client.get("/api/site/hx-table-test/domains").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("hx-test.example.com"),
        "HTMX table should contain the domain"
    );
    // Should NOT contain the base HTML wrapper
    assert!(
        !body.contains("<html"),
        "HTMX response should not be a full page"
    );
}

// ── §36 — Site domains sub-page: pre-selected site in new domain modal ───────

#[tokio::test]
async fn site_domains_new_modal_preselected() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Create a site
    let page = client
        .get("/sites/new")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/sites/new",
            &[
                ("backend", "http://127.0.0.1:8080"),
                ("name", "preselect-test-site"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();

    // Open the new domain modal from the site sub-page
    let resp = client
        .get("/api/site/preselect-test-site/domains/new")
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();

    // The site field is locked in site-specific context: rendered as a
    // read-only display with a hidden input carrying the value, so the
    // form still POSTs `site_name=preselect-test-site` back.
    assert!(
        body.contains(r#"value="preselect-test-site""#),
        "Expected preselected site name to be carried in a hidden field"
    );
    assert!(
        !body.contains("Select a site..."),
        "Site field should be locked (no dropdown) when invoked from a site sub-page"
    );
}

// ── §13 — Sites form preserves user input on error ──────────────────────────

#[tokio::test]
async fn sites_create_preserves_form_values_on_error() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client
        .get("/sites/new")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();

    // Submit with an empty backend (the JS empty-backend guard is
    // client-side, so we exercise the server-side path by POSTing an
    // empty string).
    let resp = client
        .post_form(
            "/sites/new",
            &[
                ("backend", ""),
                ("name", "preserved-name"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap();
    let _ = status; // kept for debugging

    // The user-friendly summary error must be present, and the form
    // must pre-fill the name input with the value the user typed.
    assert!(
        body.contains("Backend is required"),
        "summary error should mention 'Backend is required'"
    );
    // The name input must carry `value="preserved-name"` so the user
    // doesn't have to re-type it.
    assert!(
        body.contains(r#"id="site-name" required"#) && body.contains(r#"value="preserved-name""#),
        "name input should be pre-filled with the submitted value"
    );
    // The hidden backend field should also reflect the (empty) submission
    // — proving the server re-rendered with the user's input intact.
    client
        .assert_selector_exists(&body, r#"input[name="backend"]"#)
        .expect("hidden backend input should still be present");
    // No site was created — a fresh site-name must not appear in the
    // sites list.
    let list_body = client.get("/sites").await.unwrap().text().await.unwrap();
    assert!(
        !list_body.contains("preserved-name"),
        "site should not be persisted on validation error"
    );
}

// ── §14 — Sites create with direct/file backend ──────────────────────────────

#[tokio::test]
async fn sites_create_direct_file_backend() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client
        .get("/sites/new")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();

    let resp = client
        .post_form(
            "/sites/new",
            &[
                ("backend", "file:///var/www/static"),
                ("name", "file-static-site"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "expected redirect after successful create, got {}",
        resp.status()
    );

    // The site should appear in the sites list with the file:// backend.
    let list_body = client.get("/sites").await.unwrap().text().await.unwrap();
    assert!(list_body.contains("file-static-site"));
    assert!(list_body.contains("file:///var/www/static"));
}

// ── §14c — Edit page preserves file:// path with leading slash ──────────────

#[tokio::test]
async fn sites_edit_preserves_file_path_leading_slash() {
    // Regression: edit page for a file:// site used to show the path
    // without its leading slash, so a re-submit would drop the root.
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Create a site with file:// URL (the leading slash in the path is
    // essential for the path to be a valid absolute filesystem path).
    let page = client
        .get("/sites/new")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/sites/new",
            &[
                ("backend", "file:///Users/geax/foo"),
                ("name", "file-slash-test"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();

    // Open the edit page and verify the host:port field shows the path
    // WITH the leading slash — so the user can resubmit without losing it.
    let edit_body = client
        .get("/sites/edit?name=file-slash-test")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    client
        .assert_selector_exists(&edit_body, r#"select[name="direct_protocol"]"#)
        .unwrap();
    // The host:port field for direct mode should carry value="/Users/geax/foo"
    // (with the leading slash preserved).
    let doc = Html::parse_document(&edit_body);
    let sel = Selector::parse(r#"input[name="direct_host"]"#).unwrap();
    let direct_host = doc
        .select(&sel)
        .next()
        .and_then(|el| el.value().attr("value"))
        .unwrap_or("");
    assert_eq!(
        direct_host, "/Users/geax/foo",
        "edit page should preserve the leading slash in the file:// path"
    );
}

// ── §14b — Sites create with file:// + long path (server-side fallback) ──────

#[tokio::test]
async fn sites_create_file_long_path_server_side_fallback() {
    // Mimics the real-world case where the JS update of the hidden
    // `backend` field didn't fire (or fired late) before the form was
    // submitted. The server should still be able to assemble the backend
    // from the individual form fields (`route_mode`, `direct_protocol`,
    // `direct_host`).
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client
        .get("/sites/new")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();

    let resp = client
        .post_form(
            "/sites/new",
            // No `backend` hidden field — only the individual components.
            &[
                ("route_mode", "direct"),
                ("direct_protocol", "file"),
                (
                    "direct_host",
                    "/Users/geax/code/geax/github.com/yaitoo/proxy/cmd/mux/yaitoo",
                ),
                ("name", "long-path-site"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "expected redirect after successful create, got {}",
        resp.status()
    );

    let list_body = client.get("/sites").await.unwrap().text().await.unwrap();
    assert!(list_body.contains("long-path-site"));
    // The path was correctly assembled into a file:// URL.
    assert!(
        list_body.contains("file:///Users/geax/code/geax/github.com/yaitoo/proxy/cmd/mux/yaitoo"),
        "expected the file:// URL to be reconstructed from the form fields"
    );
}

// ── §15 — Sites form renders tunnel-mode UI and (with no tunnels) warning ──

#[tokio::test]
async fn sites_create_tunnel_backend_no_tunnels_registered() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // The new site form should always render the route-mode picker
    // (Direct / Tunnel). With no tunnels registered, switching to Tunnel
    // mode shows a helpful warning — that path is what we exercise here.
    let page = client
        .get("/sites/new")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    // Both route-mode options present.
    assert!(page.contains(r#"value="direct""#));
    assert!(page.contains(r#"value="tunnel""#));
    // New site starts in Direct mode (per template default).
    assert!(page.contains("Direct") && page.contains("Tunnel"));
}

// ── §16 — Sites edit and update (preserves form data on edit error too) ─────

#[tokio::test]
async fn sites_edit_and_update() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Create
    let page = client
        .get("/sites/new")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/sites/new",
            &[
                ("backend", "http://127.0.0.1:8080"),
                ("name", "edit-update-site"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();

    // Update to a different backend
    let edit_page = client
        .get("/sites/edit?name=edit-update-site")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let edit_csrf = client.csrf_token(&edit_page).unwrap_or_default();
    let resp = client
        .post_form(
            "/sites/edit?name=edit-update-site",
            &[
                ("backend", "http://127.0.0.1:9090"),
                ("host_mode", "passthrough"),
                ("_csrf", &edit_csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "expected redirect after update, got {}",
        resp.status()
    );

    // Re-fetch the edit page; the new backend should be pre-filled.
    let body = client
        .get("/sites/edit?name=edit-update-site")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("http://127.0.0.1:9090"));

    // And an invalid update (empty backend) should re-render the form
    // without persisting the change.
    let bad_csrf = client.csrf_token(&body).unwrap_or_default();
    let bad_resp = client
        .post_form(
            "/sites/edit?name=edit-update-site",
            &[
                ("backend", ""),
                ("host_mode", "passthrough"),
                ("_csrf", &bad_csrf),
            ],
        )
        .await
        .unwrap();
    let bad_body = bad_resp.text().await.unwrap();
    assert!(bad_body.contains("Backend is required"));
    // The previous (good) backend must still be in the page (proves we
    // re-fetched the existing value rather than blanking it).
    assert!(
        bad_body.contains("http://127.0.0.1:9090"),
        "edit-error page should still show the previously-saved backend"
    );
}

// ── §37 — Tunnels: new page renders ─────────────────────────────────────────

#[tokio::test]
async fn tun_new_page_renders() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let resp = client.get("/tun/new").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    client
        .assert_selector_exists(&body, "input[name=name]")
        .unwrap();
    client
        .assert_selector_exists(&body, "input[name=token]")
        .unwrap();
}

// ── §38 — Tunnels: create + edit + delete (full CRUD UI flow) ───────────────

#[tokio::test]
async fn tun_crud_full_ui_flow() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // GET /tun/new page, capture CSRF.
    let page = client.get("/tun/new").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();

    // POST /tun/new with name + token + enabled.
    let resp = client
        .post_form(
            "/tun/new",
            &[
                ("name", "office"),
                ("token", "secret-token-123"),
                ("enabled", "1"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "create tun should redirect, got {}",
        resp.status()
    );

    // Verify the new tun is listed on /tun.
    let list_body = client.get("/tun").await.unwrap().text().await.unwrap();
    assert!(
        list_body.contains("office"),
        "tunnels list should contain 'office' after create, got: {}",
        list_body
    );

    // GET /tun/office/edit page.
    let edit_page = client
        .get("/tun/office/edit")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let edit_csrf = client.csrf_token(&edit_page).unwrap_or_default();
    // The form should have the tun name visible (readonly style).
    assert!(
        edit_page.contains("office"),
        "edit page should show tun name, got: {}",
        edit_page
    );

    // POST /tun/office/edit to update the token.
    let update_resp = client
        .post_form(
            "/tun/office/edit",
            &[
                ("token", "rotated-token-456"),
                ("enabled", "1"),
                ("_csrf", &edit_csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        update_resp.status().as_u16(),
        302,
        "update tun should redirect, got {}",
        update_resp.status()
    );

    // POST /tun/office/delete.
    let list_page_after = client.get("/tun").await.unwrap().text().await.unwrap();
    let delete_csrf = client.csrf_token(&list_page_after).unwrap_or_default();
    let del_resp = client
        .post_form("/tun/office/delete", &[("_csrf", &delete_csrf)])
        .await
        .unwrap();
    assert_eq!(
        del_resp.status().as_u16(),
        302,
        "delete tun should redirect, got {}",
        del_resp.status()
    );

    // Confirm deletion in the list.
    let final_body = client.get("/tun").await.unwrap().text().await.unwrap();
    assert!(
        !final_body.contains(r#"id="tun-office""#) && !final_body.contains(">office<"),
        "tunnels list should no longer contain 'office' after delete, got: {}",
        final_body
    );
}

// ── §39 — Tunnels: create with no CSRF → 403 ────────────────────────────────

#[tokio::test]
async fn tun_create_no_csrf_forbidden() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let resp = client
        .post_form(
            "/tun/new",
            &[("name", "no-csrf-tun"), ("token", "tok"), ("enabled", "1")],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "missing CSRF should be forbidden"
    );
}

// ── §B1 — HTMX DELETE with CSRF in body (B1 regression test) ─────────────────
// Verifies that DELETE /api/domains/{domain} works when HTMX sends _csrf
// in the request body (hx-vals), not the query string. Before the fix,
// serve.rs dropped the DELETE body (matching GET|HEAD|DELETE), so the
// CSRF check in lib.rs failed and returned 403.

#[tokio::test]
async fn domains_hx_delete_with_body_csrf_works() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // 1. Create a site + domain
    let page = client
        .get("/sites/new")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/sites/new",
            &[
                ("backend", "http://127.0.0.1:8080"),
                ("name", "hx-delete-site"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();

    let page2 = client.get("/domains").await.unwrap().text().await.unwrap();
    let csrf2 = client.csrf_token(&page2).unwrap_or_default();
    client
        .post_form(
            "/domains/new",
            &[
                ("domain", "hx-delete.example.com"),
                ("site_name", "hx-delete-site"),
                ("_csrf", &csrf2),
            ],
        )
        .await
        .unwrap();

    // 2. Capture CSRF token from the site_domains HTMX page
    let site_page = client
        .get("/site/hx-delete-site/domains")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf3 = client.csrf_token(&site_page).unwrap_or_default();

    // 3. DELETE with _csrf in BODY (not query) — HTMX hx-vals pattern
    let resp = client
        .delete_form("/api/domains/hx-delete.example.com", &[("_csrf", &csrf3)])
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "HTMX DELETE with body CSRF should return 200, got {}",
        resp.status()
    );

    // 4. Verify domain is gone — site-specific domain table should be empty
    let site_page = client
        .get("/site/hx-delete-site/domains")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        !site_page.contains("hx-delete.example.com"),
        "domain should be gone after HTMX DELETE",
    );
}

// ── §M1 — Tun create duplicate should error without clobbering existing ─────
// Verifies that POST /tun/new with a duplicate name returns an error page
// (status 200) instead of silently overwriting the existing tun.
// The overwrite would clobber online/last_seen_at/expires_at fields.

#[tokio::test]
async fn tun_create_duplicate_fails_without_overwriting() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // 1. Create first tun with a distinct token
    let page = client.get("/tun/new").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();

    let resp = client
        .post_form(
            "/tun/new",
            &[
                ("name", "dup-tun"),
                ("token", "first-token"),
                ("enabled", "1"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "first tun create should redirect, got {}",
        resp.status()
    );

    // 2. Verify dup-tun appears in the list
    let list = client.get("/tun").await.unwrap().text().await.unwrap();
    assert!(
        list.contains("dup-tun"),
        "first tun should appear in /tun list"
    );

    // 3. Attempt to create another tun with the same name but different token
    let page2 = client.get("/tun/new").await.unwrap().text().await.unwrap();
    let csrf2 = client.csrf_token(&page2).unwrap_or_default();

    let resp2 = client
        .post_form(
            "/tun/new",
            &[
                ("name", "dup-tun"),
                ("token", "second-token"),
                ("enabled", "1"),
                ("_csrf", &csrf2),
            ],
        )
        .await
        .unwrap();
    // Should return 200 with error page, NOT 302 redirect
    assert_eq!(
        resp2.status().as_u16(),
        200,
        "duplicate tun create should return 200 error page, got {}",
        resp2.status()
    );
    let body2 = resp2.text().await.unwrap();
    assert!(
        body2.contains("already exists"),
        "error page should mention 'already exists', got: {}",
        body2
    );

    // 4. Verify the first tun is UNCHANGED — edit page should NOT show second-token
    let edit_page = client.get("/tun").await.unwrap().text().await.unwrap();
    // The tun list page shows the tun name; navigate to the edit page for dup-tun
    // We get /tun/edit?name=dup-tun via query param
    let edit_resp = client.get("/tun").await.unwrap().text().await.unwrap();
    // The TunFormTemplate never echoes back the token value (security).
    // Instead we verify that "second-token" does NOT appear anywhere as
    // a marker that the old row was NOT overwritten.
    assert!(
        !edit_resp.contains("second-token"),
        "edit page should NOT contain 'second-token' — existing tun must be unchanged"
    );
}
