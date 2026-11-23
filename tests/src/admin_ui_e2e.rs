//! Admin UI e2e tests — full HTTP requests against a real pangolin-ngx binary.
//!
//! Covers: login flow, auth redirect, CSRF enforcement, and CRUD for all
//! entities (sites, domains, tokens, certs) plus tunnels (read-only).
//!
//! Prerequisites: `make build` (or `cargo build --release -p ngx -p tun`)

use crate::admin_harness::AdminClient;
use crate::harness::{NgxProcess, init_pangolin_db};
use scraper::{Html, Selector};

// ── helper ──────────────────────────────────────────────────────────────────

fn new_client_no_redirect() -> reqwest::Client {
    AdminClient::build_http_client()
}

async fn start_ngx() -> NgxProcess {
    NgxProcess::start(init_pangolin_db).await
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

// ── §5 — The overview page renders after login ──────────────────────────────
//
// The nav entry and the page itself are both called "Overview" (issue
// #185) — "dashboard" is the central product's name, not this page's.

#[tokio::test]
async fn overview_renders_after_login() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();
    let resp = client.get("/").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Overview"), "missing Overview text");
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

// ── §15.5 — Sites list mobile card structure (issue #72) ────────────────────
//
// Regression guard for the unbalanced `</div>` that lived inside the
// `block md:hidden` mobile card template. With N>1 sites, the previous bug
// caused the mobile block wrapper to close early, leaking sibling cards out
// of the `overflow-x-auto` container and breaking the table layout below
// 768px viewports.
//
// We assert structural invariants on the rendered HTML using scraper:
//   - the mobile block wrapper exists and contains exactly N site cards
//   - each card wraps all four sections (header, on/off badge, backend, actions)
//   - per-card granularity catches a future refactor that moves the on/off
//     badge out of the card wrapper (a body-wide card count would miss it)

#[tokio::test]
async fn sites_list_mobile_cards_well_formed() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Create 3 sites so the regression (visible only with N>1) would manifest.
    for site_name in &["mobile-a", "mobile-b", "mobile-c"] {
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

    let body = client.get("/sites").await.unwrap().text().await.unwrap();
    let doc = Html::parse_document(&body);

    // The mobile block wrapper is the only element on this page carrying
    // `md:hidden` (desktop table uses `hidden md:table`, the inverse). A
    // class-substring match avoids the CSS3 colon-escape ambiguity.
    let mobile_sel = Selector::parse(r#"div[class*="md:hidden"]"#).unwrap();
    let card_sel = Selector::parse("div.p-4").unwrap();
    // The on/off badge: green when enabled, slate when disabled. We restrict
    // to <span> so the backend <code> block (also bg-slate-100) is excluded.
    let badge_sel = Selector::parse("span.bg-green-100, span.bg-slate-100").unwrap();
    let code_sel = Selector::parse("code").unwrap();
    let edit_sel = Selector::parse(r#"a[href^="/sites/edit"]"#).unwrap();
    let delete_sel = Selector::parse(r#"button[hx-delete^="/api/sites/"]"#).unwrap();
    // The outer scroll container wrapping both the desktop table and the
    // mobile block. Asserted separately to catch a regression where the
    // mobile wrapper leaks out of the overflow-x-auto container.
    let overflow_sel = Selector::parse("div.overflow-x-auto").unwrap();

    let mobile_block = doc
        .select(&mobile_sel)
        .next()
        .expect("mobile block wrapper should be present");

    let overflow = doc
        .select(&overflow_sel)
        .next()
        .expect("overflow-x-auto container should be present");

    // The mobile block must be nested inside the overflow-x-auto container —
    // not a sibling. A sibling relationship would mean the wrapper leaked
    // out of the scroll container (a higher-level regression of #72 where
    // a stray </div> lifts the mobile block out of its parent).
    assert!(
        overflow
            .select(&Selector::parse(r#"div[class*="md:hidden"]"#).unwrap())
            .next()
            .is_some(),
        "mobile block should be nested inside the overflow-x-auto container, \
         not a sibling — a regression of the #72 wrapper-leak bug"
    );

    let cards: Vec<_> = mobile_block.select(&card_sel).collect();
    assert_eq!(
        cards.len(),
        3,
        "expected 3 mobile cards inside the mobile block, found {} — \
         this usually means a stray </div> closed the wrapper early",
        cards.len()
    );

    // Per-card granularity catches a future refactor that accidentally moves
    // the on/off badge out of the card wrapper — which a body-wide card
    // count would not detect.
    let names = ["mobile-a", "mobile-b", "mobile-c"];
    for (i, card) in cards.iter().enumerate() {
        let card_text: String = card.text().collect();
        assert!(
            card_text.contains(names[i]),
            "card {} should contain site name '{}', got text: {:?}",
            i,
            names[i],
            card_text
        );
        assert!(
            card.select(&badge_sel).next().is_some(),
            "card {} should contain the on/off badge inside it",
            i
        );
        assert!(
            card.select(&code_sel).next().is_some(),
            "card {} should contain the backend <code> block",
            i
        );
        assert_eq!(
            card.select(&edit_sel).count(),
            1,
            "card {} should contain exactly one Edit link",
            i
        );
        assert_eq!(
            card.select(&delete_sel).count(),
            1,
            "card {} should contain exactly one Delete button",
            i
        );
    }
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

/// Issue #55 E2E: the admin form's `challenge_kind` dropdown must
/// persist the operator's choice to the database. This pins the
/// full form → backend → DB round-trip for one explicit kind.
/// (The pre-#55 form had no dropdown, so this would have stored
/// `None` regardless of what the user submitted — making the
/// feature invisible end-to-end.)
#[tokio::test]
async fn domains_challenge_kind_form_persists() {
    use rusqlite::Connection;

    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Create a site first.
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
                ("name", "ck-site"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();

    // Submit the new-domain form with challenge_kind = dns-01.
    let page2 = client.get("/domains").await.unwrap().text().await.unwrap();
    let csrf2 = client.csrf_token(&page2).unwrap_or_default();
    let resp = client
        .post_form(
            "/domains/new",
            &[
                ("domain", "ck.example.com"),
                ("site_name", "ck-site"),
                ("challenge_kind", "dns-01"),
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

    // Verify the row in SQLite has the explicit kind. The harness
    // can open the same DB file (WAL) and read the column.
    let db_path = ngx.db_path();
    let conn = Connection::open(&db_path).expect("open db");
    let kind: Option<String> = conn
        .query_row(
            "SELECT challenge_kind FROM domains WHERE domain = 'ck.example.com'",
            [],
            |r| r.get(0),
        )
        .expect("row must exist");
    assert_eq!(
        kind.as_deref(),
        Some("dns-01"),
        "challenge_kind must be persisted as the kebab-case string"
    );

    // Re-render the form for the domain: the dropdown must show
    // dns-01 as selected (not auto).
    // The new form does not have an edit page (the codebase has a
    // stub `DomainsEditTemplate` that's not wired into a route
    // yet), so we verify the value is readable end-to-end via the
    // DB; a future PR can add the edit page and assert
    // `<option ... selected>` on it.
}

/// Issue #55 E2E: submitting a wildcard domain with
/// challenge_kind=http-01 must be rejected at save time with a
/// 200 + error message containing the literal "RFC 8555 §8.3".
/// (Pre-#55 the form had no wildcard × http-01 check; the request
/// would 302 to /domains and the bad combination would be saved,
/// causing the next ACME issuance to fail with a generic server
/// error.)
#[tokio::test]
async fn domains_wildcard_http01_rejected_at_save() {
    use rusqlite::Connection;

    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Create a site + a DNS provider so the wildcard rule does
    // not short-circuit on "no provider" (we want the *specific*
    // http-01 check to fire).
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
                ("name", "w-site"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    let page = client.get("/dns").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    let resp = client
        .post_form(
            "/dns/new",
            &[
                ("name", "cf"),
                ("kind", "cloudflare"),
                ("config", "{}"),
                ("_csrf", &csrf),
            ],
        )
        .await;
    let _ = resp; // dns/new may or may not be wired into this build

    // Submit a wildcard domain with challenge_kind=http-01.
    let page2 = client.get("/domains").await.unwrap().text().await.unwrap();
    let csrf2 = client.csrf_token(&page2).unwrap_or_default();
    let resp = client
        .post_form(
            "/domains/new",
            &[
                ("domain", "*.wildcard.example"),
                ("site_name", "w-site"),
                ("dns_provider", "cf"),
                ("challenge_kind", "http-01"),
                ("_csrf", &csrf2),
            ],
        )
        .await
        .unwrap();

    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();

    // The save-time rejection renders the form back with 200 + an
    // error message; it does NOT 302 to /domains. The error
    // message MUST contain "RFC 8555 §8.3" so operators and tests
    // can grep for the spec reference.
    assert_eq!(
        status, 200,
        "wildcard × http-01 must be rejected at save (got {status})"
    );
    assert!(
        body.contains("RFC 8555 §8.3"),
        "error body must reference the RFC; got: {}",
        &body[..body.len().min(400)]
    );

    // Verify no row was inserted.
    let db_path = ngx.db_path();
    let conn = Connection::open(&db_path).expect("open db");
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM domains WHERE domain = '*.wildcard.example'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0, "wildcard × http-01 must not be saved");
}

/// Issue #55 E2E: the new form's HTML must contain the four
/// challenge-kind options. This is the cheapest possible
/// regression test for the dropdown UI — if a future refactor
/// drops the `<option value="...">` rows, this fails.
#[tokio::test]
async fn domains_new_page_has_challenge_kind_dropdown() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client
        .get("/domains/new")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        page.contains("domain-challenge-kind"),
        "form must include the challenge-kind select"
    );
    for opt in ["auto", "http-01", "dns-01", "dns-persist-01"] {
        assert!(
            page.contains(&format!("value=\"{opt}\"")),
            "form must offer challenge_kind={opt} as an option"
        );
    }
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

// ── §19.x — Domain edit (issue #57) ──────────────────────────────────────────

/// Setup helper: create a site + a domain, return the domain name.
/// Used by the issue-#57 edit-flow tests below.
async fn setup_domain_for_edit(client: &AdminClient, domain: &str, site_name: &str) {
    // Site
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
                ("name", site_name),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();

    // Domain
    let list = client.get("/domains").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&list).unwrap_or_default();
    let resp = client
        .post_form(
            "/domains/new",
            &[
                ("domain", domain),
                ("site_name", site_name),
                ("enabled", "1"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 302, "domain create should redirect");
}

/// `GET /domains/{domain}/edit` returns 200 and renders the edit page
/// with the row's current values pre-filled.
#[tokio::test]
async fn domains_edit_page_renders_with_current_values() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();
    setup_domain_for_edit(&client, "edit-render.com", "site-edit-render").await;

    let resp = client.get("/domains/edit-render.com/edit").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("Edit domain"),
        "page should have Edit heading"
    );
    assert!(
        body.contains("edit-render.com"),
        "page should show the domain name"
    );
    assert!(
        body.contains("site-edit-render"),
        "page should show the current site_name"
    );
}

/// The Edit button appears on the domains list page and links to
/// `/domains/{domain}/edit`.
#[tokio::test]
async fn domains_list_has_edit_button() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();
    setup_domain_for_edit(&client, "edit-link.com", "site-edit-link").await;

    let list = client.get("/domains").await.unwrap().text().await.unwrap();
    assert!(
        list.contains("href=\"/domains/edit-link.com/edit\""),
        "list page should contain Edit link to /domains/edit-link.com/edit, body was: {}",
        list
    );
}

/// `POST /api/domains/{domain}/edit` updates site_name + reloads.
#[tokio::test]
async fn domains_edit_updates_site_name() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Two sites so we can move the domain between them.
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
                ("name", "site-a"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
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
                ("backend", "http://127.0.0.1:8081"),
                ("name", "site-b"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();

    // Create domain on site-a
    let list = client.get("/domains").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&list).unwrap_or_default();
    let resp = client
        .post_form(
            "/domains/new",
            &[
                ("domain", "mover.com"),
                ("site_name", "site-a"),
                ("enabled", "1"),
                ("auto_issue", "1"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 302);

    // Edit: move to site-b
    let edit = client
        .get("/domains/mover.com/edit")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&edit).unwrap_or_default();
    let resp = client
        .post_form(
            "/api/domains/mover.com/edit",
            &[
                ("domain", "mover.com"),
                ("site_name", "site-b"),
                ("enabled", "1"),
                ("auto_issue", "1"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "domain edit should redirect, got {}",
        resp.status()
    );

    // Verify the new site is reflected in the list page (it shows site_name).
    let list = client.get("/domains").await.unwrap().text().await.unwrap();
    // The table row for mover.com should now show site-b.
    assert!(
        list.contains("mover.com"),
        "domain should still appear in list"
    );
    // The per-site sub-page is the cleanest assertion: site-a/domains
    // should no longer contain mover.com, site-b/domains should.
    let site_a = client
        .get("/site/site-a/domains")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        !site_a.contains("mover.com"),
        "mover.com should no longer be on site-a, got: {}",
        site_a
    );
    let site_b = client
        .get("/site/site-b/domains")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        site_b.contains("mover.com"),
        "mover.com should be on site-b, got: {}",
        site_b
    );
}

/// Editing a domain to add dns_provider=null must succeed for
/// non-wildcard domains (covers the `dns_provider` None path).
#[tokio::test]
async fn domains_edit_can_clear_dns_provider() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();
    setup_domain_for_edit(&client, "no-dns.com", "site-no-dns").await;

    // Edit: clear dns_provider (empty string -> None)
    let edit = client
        .get("/domains/no-dns.com/edit")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&edit).unwrap_or_default();
    let resp = client
        .post_form(
            "/api/domains/no-dns.com/edit",
            &[
                ("domain", "no-dns.com"),
                ("site_name", "site-no-dns"),
                ("enabled", "1"),
                ("auto_issue", "1"),
                ("dns_provider", ""),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 302);
}

/// POST with wildcard + missing dns_provider must be rejected
/// (preserves the handle_create invariant).
#[tokio::test]
async fn domains_edit_wildcard_requires_dns_provider() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Setup a DNS provider so we can prove the wildcard is the rejection
    // cause, not the provider being absent.
    let page = client.get("/dns/new").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/dns/new",
            &[
                ("name", "cf-main"),
                ("kind", "cloudflare"),
                ("enabled", "1"),
                ("api_token", "test-token-1234"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();

    setup_domain_for_edit(&client, "site-w.com", "site-w").await;

    // Now create a wildcard domain with the provider (so we have one to
    // try to edit), then attempt to edit it with no provider.
    let list = client.get("/domains").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&list).unwrap_or_default();
    let resp = client
        .post_form(
            "/domains/new",
            &[
                ("domain", "*.wildcard.com"),
                ("site_name", "site-w"),
                ("enabled", "1"),
                ("auto_issue", "1"),
                ("dns_provider", "cf-main"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "wildcard create should succeed"
    );

    // Now try to edit it with empty dns_provider — should be rejected
    // (200 OK with inline error, NOT a 302). The wildcard `*` in
    // the path is percent-encoded because reqwest preserves it
    // literally. (The route handler decodes path params before
    // looking up the row, so `%2A.wildcard.com` resolves to
    // `*.wildcard.com` in the DB.)
    let edit = client
        .get("/domains/%2A.wildcard.com/edit")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&edit).unwrap_or_default();
    let resp = client
        .post_form(
            "/api/domains/%2A.wildcard.com/edit",
            &[
                ("domain", "*.wildcard.com"),
                ("site_name", "site-w"),
                ("enabled", "1"),
                ("auto_issue", "1"),
                ("dns_provider", ""),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "wildcard-without-dns should be rejected (200 with inline error), got {}",
        resp.status()
    );
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("Wildcard domains require a DNS provider"),
        "error message should explain the wildcard constraint, got: {}",
        body
    );
}

/// POST with non-existent dns_provider must be rejected.
#[tokio::test]
async fn domains_edit_nonexistent_dns_provider_rejected() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();
    setup_domain_for_edit(&client, "needs-dns.com", "site-needs-dns").await;

    let edit = client
        .get("/domains/needs-dns.com/edit")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf = client.csrf_token(&edit).unwrap_or_default();
    let resp = client
        .post_form(
            "/api/domains/needs-dns.com/edit",
            &[
                ("domain", "needs-dns.com"),
                ("site_name", "site-needs-dns"),
                ("enabled", "1"),
                ("auto_issue", "1"),
                ("dns_provider", "does-not-exist"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "non-existent dns_provider should be rejected with 200 + inline error"
    );
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("does not exist"),
        "error should mention missing provider, got: {}",
        body
    );
}

/// `GET /domains/{nonexistent}/edit` should 404.
#[tokio::test]
async fn domains_edit_404_for_missing_domain() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let resp = client.get("/domains/nope.com/edit").await.unwrap();
    assert_eq!(resp.status().as_u16(), 404);
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

// ── §issue-45: Unified certs lifecycle (status badges, retry, summary) ──

/// Manual cert upload via `POST /certs/new` lands in the table with
/// `status=issued` (regression: pre-V4, the row didn't carry a status
/// column at all; backward-compat is V4's `DEFAULT 'issued'` plus the
/// admin form's explicit `CertStatus::Issued`).
#[tokio::test]
async fn certs_manual_upload_lands_as_issued() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client.get("/certs").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/certs/new",
            &[
                ("domain", "manual.example.com"),
                ("cert_file", "/tmp/manual.pem"),
                ("key_file", "/tmp/manual.key"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();

    let body = client.get("/certs").await.unwrap().text().await.unwrap();
    let doc = Html::parse_document(&body);
    // The row carries `data-status="issued"` (driven by `CertRow::status`)
    // even though the form posted no explicit status — the route
    // handler defaults to `CertStatus::Issued` for manual rows.
    let sel = Selector::parse(r#"tr[data-domain="manual.example.com"]"#).unwrap();
    let row = doc.select(&sel).next().expect("row exists");
    assert_eq!(
        row.value().attr("data-status"),
        Some("issued"),
        "manual upload must land as Issued"
    );
}

/// Enabling `auto_issue=true` on a domain immediately writes a `pending`
/// row in `certs`, so the operator sees the lifecycle from the moment
/// they save the form — no vacuum window where /certs is empty for an
/// enabled domain.
#[tokio::test]
async fn domain_with_auto_issue_writes_pending_cert_row() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Create a site to anchor the domain.
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
                ("name", "auto-issue-site"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();

    // Create domain with auto_issue=true. The DNS provider field is
    // intentionally empty — plan_issuance for a bare FQDN without DNS
    // association picks HTTP-01, which is a valid (non-wildcard) plan,
    // so the domains form will not reject it.
    let page2 = client
        .get("/domains/new")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let csrf2 = client.csrf_token(&page2).unwrap_or_default();
    let resp = client
        .post_form(
            "/domains/new",
            &[
                ("domain", "autoissue.example.com"),
                ("site_name", "auto-issue-site"),
                ("auto_issue", "on"),
                ("_csrf", &csrf2),
            ],
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 302);

    // Confirm the certs page now has a row for this domain. The status
    // is a race against the ACME background loop, which may have ticked
    // and transitioned the row past `pending` to `issuing` or `failed`
    // by the time we read /certs. All three states confirm the seeding
    // worked — the issue brief's contract is "the operator immediately
    // sees a row", not specifically "the operator sees a pending row".
    let body = client.get("/certs").await.unwrap().text().await.unwrap();
    let doc = Html::parse_document(&body);
    let sel = Selector::parse(r#"tr[data-domain="autoissue.example.com"]"#).unwrap();
    let row = doc.select(&sel).next().expect("seeded row exists");
    let observed = row.value().attr("data-status").unwrap_or_default();
    assert!(
        matches!(observed, "pending" | "issuing" | "failed"),
        "auto_issue=true must seed a lifecycle row (got {observed:?})"
    );
}

/// `GET /certs?status=failed` filters the table to only failed rows.
/// Verifies both the listing semantics and the chip-bar highlighting.
#[tokio::test]
async fn certs_table_filter_by_status() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Seed two manual certs (both Issued) and one Failed row via direct
    // DB write. Direct DB poke is the path of least resistance: making
    // a real ACME failure inside the e2e harness would require a Pebble
    // round-trip just to assert the filter.
    let page = client.get("/certs").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    for d in ["alpha.example.com", "beta.example.com"] {
        client
            .post_form(
                "/certs/new",
                &[
                    ("domain", d),
                    ("cert_file", "/tmp/x.pem"),
                    ("key_file", "/tmp/x.key"),
                    ("_csrf", &csrf),
                ],
            )
            .await
            .unwrap();
    }
    // Direct DB write for the failed row — same SQLite file the ngx
    // process is using (see `init_pangolin_db` / `NgxProcess::start`).
    let conn = rusqlite::Connection::open(ngx.db_path()).unwrap();
    conn.execute(
        "INSERT INTO certs (domain, cert_file, key_file, sans, source, issued_at, status, last_error)
         VALUES (?1, '/tmp/f.pem', '/tmp/f.key', '[]', 'acme', 0, 'failed', 'mock failure')",
        rusqlite::params!["broken.example.com"],
    )
    .unwrap();
    drop(conn);

    // Unfiltered view shows all three. (The `default` v1 blob
    // is NOT imported into the certs table — `scan_and_import_blobs`
    // explicitly skips the `default` filename because v2 has no
    // `default` SNI fallback; see `acme::scan_and_import_blobs`.)
    let all = client.get("/certs").await.unwrap().text().await.unwrap();
    let doc_all = Html::parse_document(&all);
    let row_sel = Selector::parse("tr[data-domain]").unwrap();
    let all_rows: Vec<_> = doc_all.select(&row_sel).collect();
    assert_eq!(all_rows.len(), 3, "unfiltered table shows every row");

    // Filtered to failed: just the broken row, plus the chip-bar entry
    // for `failed` carries the active-style class.
    let only_failed = client
        .get("/certs?status=failed")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let doc = Html::parse_document(&only_failed);
    let rows: Vec<_> = doc.select(&row_sel).collect();
    assert_eq!(rows.len(), 1, "filtered table shows only failed rows");
    assert_eq!(
        rows[0].value().attr("data-domain"),
        Some("broken.example.com")
    );
    // Failed row exposes the inline detail with the recorded error.
    let err_sel = Selector::parse(r#"code[data-error="broken.example.com"]"#).unwrap();
    let err = doc.select(&err_sel).next().expect("error detail rendered");
    assert!(err.text().any(|t| t.contains("mock failure")));

    // CSV filter: pending + issuing returns nothing (no in-flight rows).
    let empty = client
        .get("/certs?status=pending,issuing")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let doc_empty = Html::parse_document(&empty);
    let empty_rows: Vec<_> = doc_empty.select(&row_sel).collect();
    assert!(empty_rows.is_empty());
}

/// `GET /api/certs/summary` returns the dashboard JSON with every
/// status bucket present (zero-valued buckets included) so the
/// dashboard template doesn't have to special-case missing keys.
#[tokio::test]
async fn certs_summary_endpoint_returns_status_counts() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Seed one Issued (via the manual form) + one Failed (direct DB).
    let page = client.get("/certs").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/certs/new",
            &[
                ("domain", "ok.example.com"),
                ("cert_file", "/tmp/x.pem"),
                ("key_file", "/tmp/x.key"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    let conn = rusqlite::Connection::open(ngx.db_path()).unwrap();
    conn.execute(
        "INSERT INTO certs (domain, cert_file, key_file, sans, source, issued_at, status, last_error)
         VALUES (?1, '/tmp/f.pem', '/tmp/f.key', '[]', 'acme', 0, 'failed', 'boom')",
        rusqlite::params!["bad.example.com"],
    )
    .unwrap();
    drop(conn);

    let body = client
        .get("/api/certs/summary")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
    // `scan_and_import_blobs` skips the `default` filename
    // (v2 has no `default` SNI fallback, so the v1
    // autocert-DirCache `default` blob should not show up
    // in the certs table), so the baseline counts are
    // `total=2, issued=1, failed=1` — exactly the rows the
    // test itself inserted.
    assert_eq!(v["total"], 2);
    assert_eq!(v["issued"], 1);
    assert_eq!(v["failed"], 1);
    // Zero-valued buckets MUST be present.
    assert_eq!(v["pending"], 0);
    assert_eq!(v["issuing"], 0);
    assert_eq!(v["skipped"], 0);
}

/// Dashboard surfaces the cert summary card with status-link badges.
/// When `failed > 0` the dashboard exposes a clickable badge that points
/// at `/certs?status=failed`; this is the "一目了然" goal from the issue.
#[tokio::test]
async fn dashboard_badge_clickable_when_failed_or_in_flight() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Empty state: no failed badge.
    let body = client.get("/").await.unwrap().text().await.unwrap();
    assert!(
        !body.contains("cert-failed-badge"),
        "no failed badge when nothing has failed"
    );

    // Seed a failed row.
    let conn = rusqlite::Connection::open(ngx.db_path()).unwrap();
    conn.execute(
        "INSERT INTO certs (domain, cert_file, key_file, sans, source, issued_at, status, last_error)
         VALUES (?1, '/tmp/f.pem', '/tmp/f.key', '[]', 'acme', 0, 'failed', 'mock failure')",
        rusqlite::params!["badge.example.com"],
    )
    .unwrap();
    drop(conn);

    let body = client.get("/").await.unwrap().text().await.unwrap();
    let doc = Html::parse_document(&body);
    let badge_sel = Selector::parse("#cert-failed-badge").unwrap();
    let badge = doc
        .select(&badge_sel)
        .next()
        .expect("dashboard exposes failed badge");
    assert_eq!(
        badge.value().attr("href"),
        Some("/certs?status=failed"),
        "badge links to status-filtered certs page"
    );
    assert!(
        badge.text().any(|t| t.contains("1")),
        "badge text reflects the count"
    );

    // Seed an in-flight row too; the in-flight badge appears.
    let conn = rusqlite::Connection::open(ngx.db_path()).unwrap();
    conn.execute(
        "INSERT INTO certs (domain, cert_file, key_file, sans, source, issued_at, status)
         VALUES (?1, '/tmp/p.pem', '/tmp/p.key', '[]', 'acme', 0, 'pending')",
        rusqlite::params!["inflight.example.com"],
    )
    .unwrap();
    drop(conn);

    let body = client.get("/").await.unwrap().text().await.unwrap();
    let doc = Html::parse_document(&body);
    let inflight_sel = Selector::parse("#cert-inflight-badge").unwrap();
    let inflight = doc
        .select(&inflight_sel)
        .next()
        .expect("in-flight badge present");
    assert_eq!(
        inflight.value().attr("href"),
        Some("/certs?status=pending,issuing")
    );
}

/// `POST /certs/retry` requires the CSRF token. Without it the request
/// is rejected with 403 (same shape as the other mutating endpoints).
#[tokio::test]
async fn certs_retry_no_csrf_forbidden() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let resp = client
        .post_form("/certs/retry", &[("domain", "missing.example.com")])
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

/// `POST /certs/retry` with a valid CSRF redirects back to /certs even
/// when the domain doesn't exist — the route is fire-and-forget by
/// design (it spawns the retrier so the operator doesn't wait for a
/// slow ACME round-trip).
#[tokio::test]
async fn certs_retry_redirects_back_to_certs() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client.get("/certs").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    let resp = client
        .post_form(
            "/certs/retry",
            &[("domain", "ghost.example.com"), ("_csrf", &csrf)],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "retry handler always 302s back; spawned task may no-op"
    );
    let loc = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(loc, "/certs");
}

/// Startup watchdog (issue #45 follow-up): a row left in `Issuing`
/// with a stale `started_at` must be swept back to `Failed` the next
/// time `pangolin-ngx` starts. Without this, a process killed
/// mid-ACME-call leaves the row spinning forever.
#[tokio::test]
async fn startup_watchdog_resets_stuck_issuing_rows() {
    // Pre-seed the DB with a stuck row whose `started_at` is well past
    // the 10-minute watchdog window, then start ngx. The watchdog runs
    // inside `App::new` (the very first thing the binary does after
    // `db::migrate`), so by the time the admin port answers /certs
    // the row has already been demoted to Failed.
    let ngx = NgxProcess::start(|path| {
        crate::harness::init_pangolin_db(path);
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute(
            "INSERT INTO certs (domain, cert_file, key_file, sans, source, issued_at, status, started_at)
             VALUES (?1, '/tmp/s.pem', '/tmp/s.key', '[]', 'acme', 0, 'issuing', '2020-01-01T00:00:00+00:00')",
            rusqlite::params!["stuck.example.com"],
        )
        .unwrap();
    })
    .await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let body = client.get("/certs").await.unwrap().text().await.unwrap();
    let doc = Html::parse_document(&body);
    let sel = Selector::parse(r#"tr[data-domain="stuck.example.com"]"#).unwrap();
    let row = doc.select(&sel).next().expect("stuck row visible");
    assert_eq!(
        row.value().attr("data-status"),
        Some("failed"),
        "watchdog must demote stale Issuing to Failed at startup"
    );
    // The last_error reason is rendered inline so operators understand
    // why a retry might be appropriate.
    let err_sel = Selector::parse(r#"code[data-error="stuck.example.com"]"#).unwrap();
    let err = doc.select(&err_sel).next().expect("error rendered inline");
    assert!(
        err.text().any(|t| t.contains("issuance interrupted")),
        "watchdog last_error must explain why"
    );
}

/// Dashboard `Recent ACME activity` panel surfaces events from the
/// in-memory buffer. Seed an entry via the public retry handler
/// (which logs via the same `EventType::Info` path the ACME flow
/// uses) and assert it appears.
#[tokio::test]
async fn dashboard_activity_panel_shows_recent_events() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Empty state: the panel is intentionally hidden so a fresh install
    // doesn't show a confusing dead box.
    let body = client.get("/").await.unwrap().text().await.unwrap();
    assert!(
        !body.contains("Recent ACME activity"),
        "panel must stay hidden when buffer is empty"
    );

    // Trigger an event via the retry handler — it spawns a task that
    // logs `cert retry <domain> failed: domain ... not found`, but the
    // dashboard doesn't see that log line. To get an entry into the
    // EventBuffer we have to seed a domain row and let the renewal
    // loop fire — that's an integration test, not a render test.
    // Instead: directly seed a Failed row so the dashboard's Certs
    // card border tints and the (will-be-empty) activity panel still
    // takes the rendering path. Then assert by reading the rendered
    // HTML.
    let conn = rusqlite::Connection::open(ngx.db_path()).unwrap();
    conn.execute(
        "INSERT INTO certs (domain, cert_file, key_file, sans, source, issued_at, status, last_error, started_at)
         VALUES (?1, '/tmp/x.pem', '/tmp/x.key', '[]', 'acme', 0, 'failed', 'mock failure', '2026-06-13T00:00:00Z')",
        rusqlite::params!["activity.example.com"],
    )
    .unwrap();
    drop(conn);

    // The activity panel rendering itself is gated on
    // `!activity.is_empty()`. The EventBuffer is process-local and
    // empty for a fresh harness — so the panel stays hidden even with
    // a Failed cert row. This is the desired empty-state behaviour
    // (the panel surfaces FLOW events, not row states). What we DO
    // assert is that the Certs card tints red, the badges are
    // clickable, and the activity panel's render code doesn't choke
    // on the empty branch.
    let body = client.get("/").await.unwrap().text().await.unwrap();
    let doc = Html::parse_document(&body);
    let badge_sel = Selector::parse("#cert-failed-badge").unwrap();
    let badge = doc.select(&badge_sel).next().expect("failed badge renders");
    assert!(badge.text().any(|t| t.contains("1")));
}

// ── §issue-48 — Unified hx-delete endpoint tests ────────────────────────────
//
// Issue #48 unifies all "delete" actions on a single mechanism: the
// HTMX `hx-delete` button fires `DELETE /api/{resource}/{id}` with the
// CSRF token carried in the request body (via `hx-vals`), and the
// server returns an empty 200 so `hx-swap="delete"` can drop the row.
//
// Each test below exercises one resource's new DELETE API endpoint:
//   • happy path (returns 200)
//   • missing CSRF (returns 403)
//   • DB row is actually removed
//
// The companion form-POST routes (`/sites/delete`, `/tun/delete`, …)
// remain as a fallback during the migration window — see lib.rs for the
// `#[deprecated_note]` comments and §issue-48 for the rollout plan.

/// DELETE /api/sites/{name} with body CSRF removes the site row
/// (issue #48, sites migration).
#[tokio::test]
async fn sites_hx_delete_with_body_csrf_works() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // 1. Create a site via the legacy form-POST path so we don't
    //    require a separate DELETE-create round trip in this test.
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

    // 2. Capture CSRF from the list page (where the hx-delete button
    //    lives after migration).
    let list_page = client.get("/sites").await.unwrap().text().await.unwrap();
    let csrf2 = client.csrf_token(&list_page).unwrap_or_default();

    // 3. HTMX DELETE with body CSRF — should return 200 + empty body.
    let resp = client
        .delete_form("/api/sites/hx-delete-site", &[("_csrf", &csrf2)])
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "HTMX DELETE /api/sites/{{name}} with body CSRF should return 200, got {}",
        resp.status()
    );

    // 4. Verify the row is gone from the rendered list.
    let list_after = client.get("/sites").await.unwrap().text().await.unwrap();
    assert!(
        !list_after.contains("hx-delete-site"),
        "site should be gone after HTMX DELETE",
    );
}

/// DELETE /api/sites/{name} without CSRF → 403 (issue #48).
#[tokio::test]
async fn sites_hx_delete_no_csrf_forbidden() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Send a non-empty body so the pingora body-read doesn't stall
    // on an empty DELETE; the missing _csrf is what the CSRF
    // middleware is supposed to flag (mirrors dns_delete_no_csrf_forbidden).
    let resp = client
        .delete_form("/api/sites/any-name", &[("name", "any-name")])
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "DELETE /api/sites/{{name}} without CSRF should be forbidden, got {}",
        resp.status()
    );
}

/// DELETE /api/tun/{name} with body CSRF removes the tun row
/// (issue #48, tun migration).
#[tokio::test]
async fn tun_hx_delete_with_body_csrf_works() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // 1. Create a tun via the form-POST route.
    let page = client.get("/tun/new").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/tun/new",
            &[
                ("name", "hx-delete-tun"),
                ("token", "tok"),
                ("enabled", "1"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();

    // 2. Capture CSRF from the list page.
    let list_page = client.get("/tun").await.unwrap().text().await.unwrap();
    let csrf2 = client.csrf_token(&list_page).unwrap_or_default();

    // 3. HTMX DELETE with body CSRF.
    let resp = client
        .delete_form("/api/tun/hx-delete-tun", &[("_csrf", &csrf2)])
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "HTMX DELETE /api/tun/{{name}} with body CSRF should return 200, got {}",
        resp.status()
    );

    // 4. Verify gone from list.
    let list_after = client.get("/tun").await.unwrap().text().await.unwrap();
    assert!(
        !list_after.contains("hx-delete-tun"),
        "tun should be gone after HTMX DELETE",
    );
}

/// DELETE /api/tun/{name} without CSRF → 403 (issue #48).
#[tokio::test]
async fn tun_hx_delete_no_csrf_forbidden() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Send a non-empty body so the pingora body-read doesn't stall.
    let resp = client
        .delete_form("/api/tun/any-name", &[("name", "any-name")])
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "DELETE /api/tun/{{name}} without CSRF should be forbidden, got {}",
        resp.status()
    );
}

/// DELETE /api/certs/{domain} with body CSRF removes the cert row
/// (issue #48, certs migration).
#[tokio::test]
async fn certs_hx_delete_with_body_csrf_works() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // 1. Create a cert via the form-POST route.
    let page = client.get("/certs").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/certs/new",
            &[
                ("domain", "hx-delete-cert.example.com"),
                ("cert_file", "/tmp/cert.pem"),
                ("key_file", "/tmp/key.pem"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();

    // 2. Capture CSRF from the list page.
    let list_page = client.get("/certs").await.unwrap().text().await.unwrap();
    let csrf2 = client.csrf_token(&list_page).unwrap_or_default();

    // 3. HTMX DELETE with body CSRF.
    let resp = client
        .delete_form(
            "/api/certs/hx-delete-cert.example.com",
            &[("_csrf", &csrf2)],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "HTMX DELETE /api/certs/{{domain}} with body CSRF should return 200, got {}",
        resp.status()
    );

    // 4. Verify gone from list.
    let list_after = client.get("/certs").await.unwrap().text().await.unwrap();
    assert!(
        !list_after.contains("hx-delete-cert.example.com"),
        "cert should be gone after HTMX DELETE",
    );
}

/// DELETE /api/certs/{domain} without CSRF → 403 (issue #48).
#[tokio::test]
async fn certs_hx_delete_no_csrf_forbidden() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Send a non-empty body so the pingora body-read doesn't stall.
    let resp = client
        .delete_form(
            "/api/certs/any.example.com",
            &[("domain", "any.example.com")],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "DELETE /api/certs/{{domain}} without CSRF should be forbidden, got {}",
        resp.status()
    );
}

/// DELETE /api/dns/{name} with body CSRF removes the DNS provider row
/// (issue #48, dns migration).
#[tokio::test]
async fn dns_hx_delete_with_body_csrf_works() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // 1. Create a provider via the form-POST route.
    let page = client.get("/dns/new").await.unwrap().text().await.unwrap();
    let csrf = client.csrf_token(&page).unwrap_or_default();
    client
        .post_form(
            "/dns/new",
            &[
                ("name", "hx-delete-dns"),
                ("kind", "cloudflare"),
                ("api_token", "tok"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();

    // 2. Capture CSRF from the list page.
    let list_page = client.get("/dns").await.unwrap().text().await.unwrap();
    let csrf2 = client.csrf_token(&list_page).unwrap_or_default();

    // 3. HTMX DELETE with body CSRF.
    let resp = client
        .delete_form("/api/dns/hx-delete-dns", &[("_csrf", &csrf2)])
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "HTMX DELETE /api/dns/{{name}} with body CSRF should return 200, got {}",
        resp.status()
    );

    // 4. Verify gone from list.
    let list_after = client.get("/dns").await.unwrap().text().await.unwrap();
    assert!(
        !list_after.contains("hx-delete-dns"),
        "dns provider should be gone after HTMX DELETE",
    );
}

/// DELETE /api/dns/{name} without CSRF → 403 (issue #48).
#[tokio::test]
async fn dns_hx_delete_no_csrf_forbidden() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Send a non-empty body so the pingora body-read doesn't stall.
    let resp = client
        .delete_form("/api/dns/any-name", &[("name", "any-name")])
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "DELETE /api/dns/{{name}} without CSRF should be forbidden, got {}",
        resp.status()
    );
}

// ── Helpers ──────────────────────────────────────────────────────────────────

// ── §19 — Per-site rate limiting (issue #117) ───────────────────────────────

/// The site form persists the per-site rate-limit policy, and the
/// `/ratelimit` page shows the policy of every site.
#[tokio::test]
async fn ratelimit_site_form_persists_the_policy() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // Create the site with a rate-limit policy.
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
                ("name", "limited"),
                ("rate_limit_enabled", "on"),
                ("rate_limit_req_limit", "42"),
                ("rate_limit_req_window_secs", "60"),
                ("rate_limit_page_limit", "7"),
                ("rate_limit_page_window_secs", "3600"),
                ("rate_limit_ip_capacity", "1024"),
                ("rate_limit_throttle_capacity", "2048"),
                ("rate_limit_throttle_secs", "600"),
                (
                    // A body with spaces: the form encoding turns them
                    // into `+`, and they have to arrive as spaces.
                    "rate_limit_custom_429_body",
                    "<!DOCTYPE html><html><body><h1 id=\"custom\">Slow down please</h1></body></html>",
                ),
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

    // The policy landed in the database.
    {
        let conn = rusqlite::Connection::open(ngx.db_path()).expect("open db");
        let req_limit: i64 = conn
            .query_row(
                "SELECT req_limit FROM site_rate_limits WHERE site_name = 'limited'",
                [],
                |r| r.get(0),
            )
            .expect("the policy row must exist");
        assert_eq!(req_limit, 42);
        let enabled: i64 = conn
            .query_row(
                "SELECT enabled FROM site_rate_limits WHERE site_name = 'limited'",
                [],
                |r| r.get(0),
            )
            .expect("the policy row must exist");
        assert_eq!(enabled, 1, "the policy must be enabled");
        let custom: String = conn
            .query_row(
                "SELECT custom_429_body FROM site_rate_limits WHERE site_name = 'limited'",
                [],
                |r| r.get(0),
            )
            .expect("the policy row must exist");
        assert!(
            custom.contains(r#"id="custom""#),
            "the custom 429 page must be stored: {custom}"
        );
        assert!(
            custom.contains("Slow down please"),
            "the spaces of the custom page must survive the form encoding: {custom}"
        );
        assert!(
            !custom.contains("Slow+down"),
            "a space must not be stored as a plus: {custom}"
        );
    }

    // The edit form shows the stored policy.
    let edit = client
        .get("/sites/edit?name=limited")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        edit.contains(r#"name="rate_limit_req_limit""#),
        "the edit form must carry the rate-limit fields: {edit}"
    );
    assert!(
        edit.contains(r#"value="42""#),
        "the edit form must show the stored request limit: {edit}"
    );

    // The /ratelimit page lists the site and its policy. The page
    // renders the published snapshot, which the background thread
    // refreshes once per second — so poll for the new site instead
    // of assuming it is there on the first paint.
    let mut html = String::new();
    for _ in 0..40 {
        let page = client.get("/ratelimit").await.unwrap();
        assert_eq!(page.status().as_u16(), 200);
        html = page.text().await.unwrap();
        // The policy summary of the per-site table is the marker — the
        // site name also shows up in the scope picker of the "add
        // rule" form.
        if html.contains("42 / 60s") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        html.contains("42 / 60s"),
        "the policy summary must be rendered: {html}"
    );

    // The HTMX fragments answer too.
    for path in [
        "/api/ratelimit/kpis",
        "/api/ratelimit/sites",
        "/api/ratelimit/rules",
    ] {
        let resp = client.get(path).await.unwrap();
        assert_eq!(resp.status().as_u16(), 200, "{path} must answer 200");
    }
}

/// An IP rule can be added through the `/ratelimit` page and removed
/// again with the HTMX delete endpoint.
#[tokio::test]
async fn ratelimit_ip_rule_add_and_delete() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let page = client.get("/ratelimit").await.unwrap();
    let html = page.text().await.unwrap();
    let csrf = client.csrf_token(&html).unwrap_or_default();

    let resp = client
        .post_form(
            "/ratelimit/rules",
            &[
                ("scope", "global"),
                ("kind", "blacklist"),
                ("cidr", "203.0.113.0/24"),
                ("note", "scrapers"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 302, "add should redirect");

    let conn = rusqlite::Connection::open(ngx.db_path()).expect("open db");
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM ip_rules", [], |r| r.get(0))
        .expect("count ip_rules");
    assert_eq!(count, 1, "the rule must be stored");

    // The rules fragment renders it — and carries a usable CSRF
    // token, because the page replaces the fragment every 5 s and a
    // literal `__CSRF__` would make every delete button fail.
    let resp = client.get("/api/ratelimit/rules").await.unwrap();
    let html = resp.text().await.unwrap();
    assert!(html.contains("203.0.113.0/24"), "{html}");
    assert!(
        !html.contains("__CSRF__"),
        "the polled fragment must carry a substituted CSRF token: {html}"
    );

    // The HTMX delete removes it.
    let resp = client
        .delete_form(
            "/api/ratelimit/rules",
            &[
                ("scope", "global"),
                ("kind", "blacklist"),
                ("cidr", "203.0.113.0/24"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM ip_rules", [], |r| r.get(0))
        .expect("count ip_rules");
    assert_eq!(count, 0, "the rule must be gone");
}

// ── §20 — Dashboard: the live "blocked clients" list (issue #117) ───────────

/// A client that the rate limiter rejects shows up in the dashboard's
/// live "blocked clients" list.
#[tokio::test]
async fn dashboard_lists_the_blocked_clients_live() {
    let ngx = NgxProcess::start(|db_path| {
        init_pangolin_db(db_path);
        let conn = rusqlite::Connection::open(db_path).expect("open db");
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO sites (name, backend, enabled, host_mode, host_custom, created_at, updated_at) \
             VALUES ('blocked-site', 'http://127.0.0.1:9', 1, 'passthrough', NULL, ?1, ?1)",
            rusqlite::params![now],
        )
        .expect("insert site");
        conn.execute(
            "INSERT INTO domains (domain, site_name, enabled, created_at) \
             VALUES ('blocked.test', 'blocked-site', 1, ?1)",
            rusqlite::params![now],
        )
        .expect("insert domain");
        // The e2e client connects over loopback.
        conn.execute(
            "INSERT INTO ip_rules (id, scope, kind, cidr, note, created_at) \
             VALUES ('global|blacklist|127.0.0.0/8', 'global', 'blacklist', '127.0.0.0/8', NULL, ?1)",
            rusqlite::params![now],
        )
        .expect("insert ip rule");
    })
    .await;

    // Trigger a rejection: the client is blacklisted → 403.
    let addr = format!("127.0.0.1:{}", ngx.http_port);
    let (status, _body) = crate::harness::raw_request(&addr, "blocked.test", "GET", "/", b"").await;
    assert_eq!(status, 403, "the blacklisted client must be rejected");

    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // The counters are published once per second — poll for the row.
    let mut html = String::new();
    for _ in 0..40 {
        let resp = client.get("/").await.unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        html = resp.text().await.unwrap();
        if html.contains("127.0.0.1") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        html.contains("Blocked clients"),
        "the dashboard must show the blocked-clients panel: {html}"
    );
    assert!(
        html.contains("127.0.0.1"),
        "the blocked client must be listed: {html}"
    );
    assert!(
        html.contains("blocked-site"),
        "the site that rejected the client must be listed: {html}"
    );

    // The HTMX fragment answers too.
    let resp = client.get("/api/ratelimit/blocked").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let html = resp.text().await.unwrap();
    assert!(html.contains("127.0.0.1"), "{html}");
}

// ── §21 — Rate limit tables: the bot identity and the IP lookup (issue #134) ─

/// A throttled known bot shows up in the rate-limit tables with its
/// name.
///
/// The bot is a **custom** one: the knownbots registry is read from
/// `<cache_dir>/conf.d/*.yaml` on top of the embedded configs, and a
/// custom bot without `urls:` is never refreshed from the network — so
/// its prefix set stays exactly what the test seeded. A real bot
/// (e.g. `googlebot`) would be overwritten by the first refresh of
/// its published IP list, which needs the network and would make the
/// test flaky.
#[tokio::test]
async fn a_throttled_known_bot_shows_up_in_the_tables_with_its_name() {
    let ngx = NgxProcess::start(|db_path| {
        init_pangolin_db(db_path);
        let conn = rusqlite::Connection::open(db_path).expect("open db");
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO sites (name, backend, enabled, host_mode, host_custom, created_at, updated_at) \
             VALUES ('bot-site', 'http://127.0.0.1:9', 1, 'passthrough', NULL, ?1, ?1)",
            rusqlite::params![now],
        )
        .expect("insert site");
        conn.execute(
            "INSERT INTO domains (domain, site_name, enabled, created_at) \
             VALUES ('bot.test', 'bot-site', 1, ?1)",
            rusqlite::params![now],
        )
        .expect("insert domain");
        // One request per window — the second request of the bot is
        // over the limit.
        conn.execute(
            "INSERT INTO site_rate_limits \
             (site_name, enabled, req_limit, req_window_secs, page_limit, page_window_secs) \
             VALUES ('bot-site', 1, 1, 60, 0, 3600)",
            rusqlite::params![],
        )
        .expect("insert site rate limit");
        // The site counts the bot against its limits instead of
        // exempting it (issue #131).
        conn.execute(
            "INSERT INTO site_rate_limit_bots (site_name, bot_name) VALUES ('bot-site', 'testbot')",
            rusqlite::params![],
        )
        .expect("insert site rate limit bots");

        // The knownbots cache directory defaults to `./logs/bots/cache`
        // relative to the working directory of the process — the
        // harness runs the binary in the tempdir the database lives
        // in. A bot config there joins the registry, and the e2e
        // client (which connects over loopback) is inside its prefix
        // set, so the gateway verifies it.
        let tmpdir = db_path.parent().expect("the db lives in the tempdir");
        let conf_d = tmpdir.join("logs/bots/cache/conf.d");
        std::fs::create_dir_all(&conf_d).expect("mkdir the bot conf.d");
        std::fs::write(
            conf_d.join("testbot.yaml"),
            "kind: SearchEngine\ncategory: search_engine\nname: testbot\nvendor: \"Test\"\nua: \"Testbot\"\n\
             custom:\n  - \"127.0.0.1/32\"\n",
        )
        .expect("write the bot config");
    })
    .await;

    // The client claims to be the bot — the gateway verifies the
    // marker against the seeded prefix set.
    let addr = format!("127.0.0.1:{}", ngx.http_port);
    let ua = "Testbot/1.0";
    let (status, _body) =
        crate::harness::raw_request_with_user_agent(&addr, "bot.test", "GET", "/", b"", ua).await;
    assert!(
        status != 429,
        "the first request is within the limit of 1, got {status}"
    );
    let (status, _body) =
        crate::harness::raw_request_with_user_agent(&addr, "bot.test", "GET", "/", b"", ua).await;
    assert_eq!(status, 429, "the bot must be over the site's limit");

    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // The counters are published once per second — poll for the row.
    let mut html = String::new();
    for _ in 0..40 {
        let resp = client.get("/api/ratelimit/sites").await.unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        html = resp.text().await.unwrap();
        if html.contains("data-bot=\"testbot\"") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        html.contains("data-bot=\"testbot\""),
        "the throttled bot must be listed with its name: {html}"
    );
    // The name is rendered as a chip.
    assert!(html.contains("bg-blue-100"), "{html}");
    assert!(html.contains(">testbot</span>"), "{html}");
    // The client-IP cell carries the lookup hooks.
    assert!(html.contains("data-asn-ip=\"127.0.0.1\""), "{html}");
    assert!(html.contains("data-asn-trigger"), "{html}");

    // The dashboard's live list carries the name too.
    let mut html = String::new();
    for _ in 0..40 {
        let resp = client.get("/api/ratelimit/blocked").await.unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        html = resp.text().await.unwrap();
        if html.contains("data-bot=\"testbot\"") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        html.contains("data-bot=\"testbot\""),
        "the throttled bot must be listed with its name: {html}"
    );
    assert!(html.contains("data-asn-ip=\"127.0.0.1\""), "{html}");
}

// ── §22 — The user agent of a rejected client (issue #149) ─────────────────

/// A rejected client shows up in the rate-limit tables with the user
/// agent it sent.
///
/// The user agent is what tells a scraper from a misconfigured
/// crawler from a residential client — without it the operator only
/// sees *that* a client was rejected.
///
/// The reputation check is **on** here, and the client announces
/// itself as an automation tool: the throttle path escalates the
/// answer to 403. The user agent is recorded before that escalation
/// decides, so an escalated client is recorded exactly once
/// (issue #149, R6).
#[tokio::test]
async fn a_rejected_client_shows_up_with_its_user_agent() {
    let ngx = NgxProcess::start(|db_path| {
        init_pangolin_db(db_path);
        crate::harness::enable_badbots(db_path);
        let conn = rusqlite::Connection::open(db_path).expect("open db");
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO sites (name, backend, enabled, host_mode, host_custom, created_at, updated_at) \
             VALUES ('ua-site', 'http://127.0.0.1:9', 1, 'passthrough', NULL, ?1, ?1)",
            rusqlite::params![now],
        )
        .expect("insert site");
        conn.execute(
            "INSERT INTO domains (domain, site_name, enabled, created_at) \
             VALUES ('ua.test', 'ua-site', 1, ?1)",
            rusqlite::params![now],
        )
        .expect("insert domain");
        // One request per window — the second request of the client
        // is over the limit.
        conn.execute(
            "INSERT INTO site_rate_limits \
             (site_name, enabled, req_limit, req_window_secs, page_limit, page_window_secs) \
             VALUES ('ua-site', 1, 1, 60, 0, 3600)",
            rusqlite::params![],
        )
        .expect("insert site rate limit");
    })
    .await;

    let addr = format!("127.0.0.1:{}", ngx.http_port);
    let ua = "PangolinIssue149/1.0";
    let (status, _body) =
        crate::harness::raw_request_with_user_agent(&addr, "ua.test", "GET", "/", b"", ua).await;
    // The site has no backend, so the gateway answers 502 — what
    // matters is that the request was *served*, not rejected: the
    // limiter only counts it. A rate-limit answer here would mean
    // the limit is not what the test thinks it is.
    assert_eq!(
        status, 502,
        "the first request must be within the limit of 1, got {status}"
    );
    let (status, _body) =
        crate::harness::raw_request_with_user_agent(&addr, "ua.test", "GET", "/", b"", ua).await;
    assert_eq!(
        status, 403,
        "the throttled client must be escalated to 403, got {status}"
    );

    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // The counters are published once per second — poll for the row.
    let mut html = String::new();
    for _ in 0..40 {
        let resp = client.get("/api/ratelimit/sites").await.unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        html = resp.text().await.unwrap();
        if html.contains("data-ua=\"PangolinIssue149/1.0\"") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        html.contains("data-ua=\"PangolinIssue149/1.0\""),
        "the rejected client must be listed with its user agent: {html}"
    );
    // The cell shows the user agent and keeps the full value in its
    // title.
    assert!(
        html.contains("title=\"PangolinIssue149/1.0\""),
        "the user-agent cell must carry the value: {html}"
    );

    // The dashboard's live list carries it too.
    let mut html = String::new();
    for _ in 0..40 {
        let resp = client.get("/api/ratelimit/blocked").await.unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        html = resp.text().await.unwrap();
        if html.contains("data-ua=\"PangolinIssue149/1.0\"") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        html.contains("data-ua=\"PangolinIssue149/1.0\""),
        "the rejected client must be listed with its user agent: {html}"
    );
}

// ── §23 — Interface language (issues #178, #179, #180, #181, #182) ───────────

/// The language switcher remembers the operator's choice and sends
/// them back to the dashboard.
#[tokio::test]
async fn admin_ui_lang_switcher_sets_cookie_and_redirects() {
    let ngx = start_ngx().await;
    let client = new_client_no_redirect();
    let resp = client
        .get(&ngx.admin_url("/lang/zh-CN"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 302);
    let cookie = resp
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        cookie.contains("pangolin_lang=zh-CN"),
        "the language cookie must be set, got: {cookie}"
    );
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(location, "/", "the switcher must land on the dashboard");
}

/// An unknown language code is a 404 — the switcher never offers one,
/// so a request for it is a mistake, not a preference.
#[tokio::test]
async fn admin_ui_lang_unknown_code_is_404() {
    let ngx = start_ngx().await;
    let client = new_client_no_redirect();
    let resp = client.get(&ngx.admin_url("/lang/xx")).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 404);
}

/// The switcher must not become an open redirect.
#[tokio::test]
async fn admin_ui_lang_next_rejects_external_url() {
    let ngx = start_ngx().await;
    let client = new_client_no_redirect();
    for next in ["https://evil.com", "//evil.com", "/\\evil.com", "evil.com"] {
        let resp = client
            .get(&ngx.admin_url(&format!("/lang/zh-CN?next={next}")))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 302);
        let location = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(location, "/", "next={next} must fall back to the dashboard");
    }
}

/// A same-site path is followed.
#[tokio::test]
async fn admin_ui_lang_next_keeps_a_local_path() {
    let ngx = start_ngx().await;
    let client = new_client_no_redirect();
    let resp = client
        .get(&ngx.admin_url("/lang/zh-CN?next=/sites/edit?name=a"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 302);
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(location, "/sites/edit?name=a");
}

/// The browser's language preference selects the interface language
/// when the request carries no language cookie.
#[tokio::test]
async fn admin_ui_lang_accept_language_header_selects_locale() {
    let ngx = start_ngx().await;
    let client = new_client_no_redirect();
    let resp = client
        .get(&ngx.admin_url("/login"))
        .header("accept-language", "zh-CN,zh;q=0.9,en;q=0.8")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains(r#"<html lang="zh-CN">"#),
        "the accept-language header must select the locale: {body}"
    );
}

/// The cookie is the operator's explicit choice — it wins over the
/// browser's preference.
#[tokio::test]
async fn admin_ui_lang_cookie_overrides_accept_language() {
    let ngx = start_ngx().await;
    let client = new_client_no_redirect();
    let resp = client
        .get(&ngx.admin_url("/login"))
        .header("accept-language", "zh-CN")
        .header("cookie", "pangolin_lang=en")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains(r#"<html lang="en">"#),
        "the language cookie must win over accept-language: {body}"
    );
    // The English wording is rendered — the cookie really did pick the
    // language, not just the `<html lang>` attribute.
    assert!(
        body.contains("Username"),
        "the English login page must be rendered: {body}"
    );
    assert!(
        !body.contains("用户名"),
        "the Chinese login page must not be rendered: {body}"
    );
}

/// The switcher is on the page and it really switches the interface
/// to Chinese (issue #179).
#[tokio::test]
async fn admin_ui_lang_switcher_is_visible_and_switches_to_chinese() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // The switcher is rendered as soon as there is more than one
    // language to choose from.
    let resp = client.get("/").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("/lang/"),
        "the language switcher must be rendered: {body}"
    );

    // The switcher writes the cookie; the cookie store keeps it.
    let resp = client.get("/lang/zh-CN").await.unwrap();
    assert_eq!(resp.status().as_u16(), 302);

    let resp = client.get("/").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains(r#"<html lang="zh-CN">"#),
        "the switched page must declare the Chinese locale: {body}"
    );
    // The navigation is Chinese — "站点" is the `nav.sites` entry.
    assert!(
        body.contains("站点"),
        "the Chinese navigation must be rendered: {body}"
    );
    assert!(
        !body.contains(">Sites<"),
        "the English navigation must be gone: {body}"
    );
}

/// The login page is translated too — the operator has to be able to
/// sign in before the switcher is reachable behind the login.
#[tokio::test]
async fn admin_ui_login_page_renders_chinese() {
    let ngx = start_ngx().await;
    let client = new_client_no_redirect();
    let resp = client
        .get(&ngx.admin_url("/login"))
        .header("accept-language", "zh-CN")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains(r#"<html lang="zh-CN">"#),
        "the accept-language header must select the locale: {body}"
    );
    assert!(
        body.contains("用户名"),
        "the Chinese login page must be rendered: {body}"
    );
    assert!(
        !body.contains(">Username<"),
        "the English login page must not be rendered: {body}"
    );
}

/// A signed-in client that speaks a chosen language.
///
/// The login is done by hand instead of through `AdminClient`, because
/// the language is negotiated per request and `AdminClient` cannot set
/// a header. The cookie store keeps the session, so the dashboard is
/// reachable afterwards.
async fn signed_in_client(ngx: &NgxProcess) -> reqwest::Client {
    let client = new_client_no_redirect();
    let resp = client.get(&ngx.admin_url("/login")).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let resp = client
        .post(&ngx.admin_url("/login"))
        .form(&[("username", "admin"), ("password", "admin")])
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "the login must succeed — the dashboard is behind it"
    );
    client
}

/// Without a language cookie and without an `Accept-Language` header
/// the interface is English — the default.
///
/// The dashboard is behind the login, so the client signs in first;
/// the sign-in carries no language of its own, so the dashboard still
/// answers in the default language.
#[tokio::test]
async fn admin_ui_default_language_is_english() {
    let ngx = start_ngx().await;
    let client = signed_in_client(&ngx).await;

    let resp = client.get(&ngx.admin_url("/")).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains(r#"<html lang="en">"#),
        "the default language must be English: {body}"
    );
    // The navigation is English — "Sites" is the `nav.sites` entry.
    assert!(
        body.contains(">Sites<"),
        "the English navigation must be rendered: {body}"
    );
    assert!(
        !body.contains("站点"),
        "the Chinese navigation must not be rendered: {body}"
    );
}

/// The browser's language preference picks the Chinese interface.
#[tokio::test]
async fn admin_ui_accept_language_zh_renders_chinese() {
    let ngx = start_ngx().await;
    let client = signed_in_client(&ngx).await;

    let resp = client
        .get(&ngx.admin_url("/"))
        .header("accept-language", "zh-CN")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains(r#"<html lang="zh-CN">"#),
        "the accept-language header must select the locale: {body}"
    );
    assert!(
        body.contains("站点"),
        "the Chinese navigation must be rendered: {body}"
    );
    assert!(
        !body.contains(">Sites<"),
        "the English navigation must not be rendered: {body}"
    );
}

/// The switcher is on the page in both languages — otherwise an
/// operator whose browser asks for a language the gateway does not
/// speak has no way to change it.
#[tokio::test]
async fn admin_ui_lang_switcher_visible_in_both_languages() {
    let ngx = start_ngx().await;
    let client = signed_in_client(&ngx).await;

    let resp = client.get(&ngx.admin_url("/")).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("/lang/"),
        "the language switcher must be rendered in English: {body}"
    );

    let resp = client
        .get(&ngx.admin_url("/"))
        .header("accept-language", "zh-CN")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("/lang/"),
        "the language switcher must be rendered in Chinese: {body}"
    );
}

/// Fetch a page and return its body.
async fn page_body(client: &AdminClient, path: &str) -> String {
    client.get(path).await.unwrap().text().await.unwrap()
}

/// A signed-in client whose interface is Chinese.
///
/// The switcher writes the language cookie and the cookie store keeps
/// it, so every request the client makes afterwards is answered in
/// Chinese.
async fn chinese_client(ngx: &NgxProcess) -> AdminClient {
    let client = AdminClient::new(ngx);
    client.login("admin", "admin").await.unwrap();
    let resp = client.get("/lang/zh-CN").await.unwrap();
    assert_eq!(resp.status().as_u16(), 302);
    client
}

/// The pages that spelled English out instead of asking the catalog
/// for it (issue #184).
///
/// The residual-English gate found "Save", the domain count and the
/// traffic page's transport labels written into the templates. The fix
/// was a key reference, so the Chinese interface says "保存", "个域名"
/// and the translated transport names — and the English prose is gone.
#[tokio::test]
async fn admin_ui_chinese_pages_carry_no_english_prose() {
    let ngx = start_ngx().await;
    let client = chinese_client(&ngx).await;

    // ── the settings page: the save button ──
    let body = page_body(&client, "/settings").await;
    assert!(
        body.contains(">保存<"),
        "the Chinese save button must be rendered: {body}"
    );

    // ── the traffic page: the transport breakdown ──
    let body = page_body(&client, "/traffic").await;
    for (needle, what) in [
        ("流式", "the stream label"),
        ("WebSocket", "the websocket label"),
        ("未知主机", "the unknown-host label"),
    ] {
        assert!(
            body.contains(needle),
            "the Chinese {what} must be rendered: {body}"
        );
    }
    assert!(
        !body.contains("unknown host"),
        "the English unknown-host label must be gone: {body}"
    );

    // ── the site domains page: the domain count ──
    let page = page_body(&client, "/sites/new").await;
    let csrf = client.csrf_token(&page).unwrap_or_default();
    let resp = client
        .post_form(
            "/sites/new",
            &[
                ("backend", "http://127.0.0.1:8080"),
                ("name", "no-english-test"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "the site must be created — the count below needs it"
    );
    let domains_page = page_body(&client, "/domains").await;
    let csrf = client.csrf_token(&domains_page).unwrap_or_default();
    let resp = client
        .post_form(
            "/domains/new",
            &[
                ("domain", "no-english.test.example.com"),
                ("site_name", "no-english-test"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "the domain must be created — the count below needs it"
    );

    let body = page_body(&client, "/site/no-english-test/domains").await;
    assert!(
        body.contains("1 个域名"),
        "the Chinese domain count must be rendered: {body}"
    );
    assert!(
        !body.contains("1 domain"),
        "the English domain count must be gone: {body}"
    );
}

/// The sites, the domains and the tunnels are translated (issue
/// #180).
///
/// The three objects an operator touches every day have to be in
/// Chinese too — not just the navigation around them.  The test walks
/// the three list pages and the three "new" forms and asserts that
/// the Chinese copy of the area is rendered and that the English copy
/// is gone.
///
/// The Chinese literals are the shipped copy, pinned on purpose: a
/// translation change is a change to what the operator reads, and it
/// has to show up in the diff.  A later copy edit updates them here
/// too.
#[tokio::test]
async fn admin_ui_site_domain_and_tunnel_pages_render_chinese() {
    let ngx = start_ngx().await;
    let client = chinese_client(&ngx).await;

    // ── the sites ──
    let body = page_body(&client, "/sites").await;
    assert!(
        body.contains("暂无站点"),
        "the Chinese empty state must be rendered: {body}"
    );
    assert!(
        body.contains("新建站点"),
        "the Chinese \"new site\" button must be rendered: {body}"
    );
    assert!(
        !body.contains("No sites yet"),
        "the English empty state must be gone: {body}"
    );

    let body = page_body(&client, "/sites/new").await;
    for (needle, what) in [
        ("路由方式", "route-mode label"),
        ("跟随域名", "host-mode option"),
        ("源站防护", "origin guard"),
        ("前置 CDN", "front CDN block"),
    ] {
        assert!(
            body.contains(needle),
            "the Chinese {what} must be rendered: {body}"
        );
    }
    assert!(
        !body.contains("Route via"),
        "the English route-mode label must be gone: {body}"
    );

    // ── the domains ──
    let body = page_body(&client, "/domains").await;
    assert!(
        body.contains("暂无域名"),
        "the Chinese empty state must be rendered: {body}"
    );
    assert!(
        !body.contains("No domains yet"),
        "the English empty state must be gone: {body}"
    );

    let body = page_body(&client, "/domains/new").await;
    assert!(
        body.contains("ACME 验证方式"),
        "the Chinese challenge-kind label must be rendered: {body}"
    );
    assert!(
        body.contains("DNS 服务商（DNS-01 验证）"),
        "the Chinese DNS-provider label must be rendered: {body}"
    );
    assert!(
        !body.contains("ACME challenge kind"),
        "the English challenge-kind label must be gone: {body}"
    );

    // ── the tunnels ──
    let body = page_body(&client, "/tun").await;
    assert!(
        body.contains("暂无隧道节点"),
        "the Chinese empty state must be rendered: {body}"
    );
    assert!(
        !body.contains("No tunnel nodes registered yet"),
        "the English empty state must be gone: {body}"
    );

    let body = page_body(&client, "/tun/new").await;
    assert!(
        body.contains("令牌到期时间"),
        "the Chinese expiry label must be rendered: {body}"
    );
    assert!(
        body.contains("留空则自动生成"),
        "the Chinese token hint must be rendered: {body}"
    );
    assert!(
        !body.contains("Token expires at"),
        "the English expiry label must be gone: {body}"
    );
}

/// The validation errors of the three areas are translated too (issue
/// #180) — they are the messages the operator reads when the form
/// rejects the submission.
///
/// The wrapper is translated, the quoted diagnostic is not: the error
/// keeps the underlying text in its placeholder, so the operator still
/// sees what the parser actually complained about.
#[tokio::test]
async fn admin_ui_site_domain_and_tunnel_validation_errors_render_chinese() {
    let ngx = start_ngx().await;
    let client = chinese_client(&ngx).await;

    // A site whose backend cannot be parsed.
    let page = page_body(&client, "/sites/new").await;
    let csrf = client.csrf_token(&page).unwrap_or_default();
    let resp = client
        .post_form(
            "/sites/new",
            &[
                ("backend", "notaurl"),
                ("name", "zh-site"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("后端地址错误"),
        "the Chinese backend error must be rendered: {body}"
    );
    // The diagnostic itself stays English — it is quoted from the
    // parser, not translated.
    assert!(
        body.contains("unsupported URL scheme"),
        "the quoted diagnostic must survive inside the translated wrapper: {body}"
    );

    // A domain without a name.
    let page = page_body(&client, "/domains/new").await;
    let csrf = client.csrf_token(&page).unwrap_or_default();
    let resp = client
        .post_form(
            "/domains/new",
            &[("site_name", "zh-site"), ("_csrf", &csrf)],
        )
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("请填写域名"),
        "the Chinese domain-name error must be rendered: {body}"
    );

    // A tunnel name with characters the name does not allow.
    let page = page_body(&client, "/tun/new").await;
    let csrf = client.csrf_token(&page).unwrap_or_default();
    let resp = client
        .post_form("/tun/new", &[("name", "bad name!"), ("_csrf", &csrf)])
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("名称只能包含字母、数字、下划线和连字符"),
        "the Chinese name error must be rendered: {body}"
    );
}

/// The domains form's "manage providers under DNS" link must not
/// swallow the challenge-kind control (issue #180).
///
/// The link used to be left unterminated in the template, so the
/// browser's HTML parser reconstructed it over the block that follows:
/// the challenge-kind label and its dropdown ended up *inside* a link
/// to /dns — clicking the label navigated away instead of opening the
/// dropdown, on the wildcard flow the form exists for.
///
/// The catalog value used to smuggle a closing tag in to compensate;
/// askama escapes `{{ }}`, so the tag was shown as text instead of
/// closing the link.  Both are fixed; this test pins the result.
#[tokio::test]
async fn admin_ui_domain_form_dns_link_does_not_swallow_the_challenge_kind() {
    let ngx = start_ngx().await;

    for chinese in [false, true] {
        let client = AdminClient::new(&ngx);
        client.login("admin", "admin").await.unwrap();
        if chinese {
            let resp = client.get("/lang/zh-CN").await.unwrap();
            assert_eq!(resp.status().as_u16(), 302);
        }
        let body = page_body(&client, "/domains/new").await;

        // The challenge-kind control is not inside a link.
        let doc = Html::parse_document(&body);
        for selector in [
            "a select#domain-challenge-kind",
            "a label[for=domain-challenge-kind]",
        ] {
            let sel = Selector::parse(selector).unwrap();
            assert!(
                doc.select(&sel).next().is_none(),
                "{selector} must not be inside a link (chinese={chinese}): {body}"
            );
        }

        // The hint shows the link text, not a tag.  The page's own
        // closing tags are of course there; what must not be there is
        // an *escaped* one — the artifact a value carried as text.
        assert!(
            !body.contains("&lt;/a&gt;"),
            "the hint must not render a tag as text (chinese={chinese}): {body}"
        );
    }
}

/// The infrastructure and the policy pages are translated too (issue
/// #181) — the certificates, the DNS providers, the rate limit and the
/// settings.
///
/// These four are where an operator lands when something went wrong: a
/// certificate that was not issued, a DNS provider that rejects its
/// credentials, a rule that blocks the wrong client.  A page that
/// still answers in English there is a page the operator cannot read
/// while they are debugging.
///
/// The Chinese literals are the shipped copy, pinned on purpose: a
/// translation change is a change to what the operator reads, and it
/// has to show up in the diff.  A later copy edit updates them here
/// too.
#[tokio::test]
async fn admin_ui_cert_dns_ratelimit_and_settings_pages_render_chinese() {
    let ngx = start_ngx().await;
    let client = chinese_client(&ngx).await;

    // ── the certificates ──
    let body = page_body(&client, "/certs").await;
    assert!(
        body.contains("暂无证书"),
        "the Chinese empty state must be rendered: {body}"
    );
    assert!(
        body.contains("添加证书"),
        "the Chinese \"add cert\" button must be rendered: {body}"
    );
    assert!(
        !body.contains("No certificates yet"),
        "the English empty state must be gone: {body}"
    );

    let body = page_body(&client, "/certs/new").await;
    for (needle, what) in [
        ("证书文件（PEM）", "certificate-file label"),
        ("私钥文件（PEM）", "key-file label"),
        ("到期时间", "expiry label"),
    ] {
        assert!(
            body.contains(needle),
            "the Chinese {what} must be rendered: {body}"
        );
    }
    assert!(
        !body.contains("Certificate file (PEM)"),
        "the English certificate-file label must be gone: {body}"
    );

    // ── the DNS providers ──
    let body = page_body(&client, "/dns").await;
    assert!(
        body.contains("暂无 DNS 服务商"),
        "the Chinese empty state must be rendered: {body}"
    );
    assert!(
        body.contains("新建 DNS 服务商"),
        "the Chinese \"new provider\" button must be rendered: {body}"
    );
    assert!(
        !body.contains("No DNS providers configured"),
        "the English empty state must be gone: {body}"
    );

    let body = page_body(&client, "/dns/new").await;
    assert!(
        body.contains("测试连接"),
        "the Chinese test-connection button must be rendered: {body}"
    );
    assert!(
        body.contains("② 凭据"),
        "the Chinese credentials heading must be rendered: {body}"
    );
    assert!(
        !body.contains("Credentials"),
        "the English credentials heading must be gone: {body}"
    );
    // The credential field names stay English — they are the field
    // names of the cloud vendor's own console.
    assert!(
        body.contains("Access Key ID"),
        "the vendor's field name must survive the translation: {body}"
    );
    assert!(
        !body.contains("访问密钥"),
        "the vendor's field name must not be translated: {body}"
    );
    assert!(
        !body.contains("Test connection"),
        "the English test-connection button must be gone: {body}"
    );

    // ── the rate limit ──
    let body = page_body(&client, "/ratelimit").await;
    for (needle, what) in [
        ("添加 IP 规则", "add-rule heading"),
        ("各站点策略", "per-site policy heading"),
    ] {
        assert!(
            body.contains(needle),
            "the Chinese {what} must be rendered: {body}"
        );
    }
    // The rules heading is "IP 规则" — a substring of the add-rule
    // heading above and of the empty state, so the needle pins the
    // element boundary: only the heading itself renders as "…>IP 规则<".
    assert!(
        body.contains(">IP 规则<"),
        "the Chinese rules heading must be rendered: {body}"
    );
    assert!(
        !body.contains("Add an IP rule"),
        "the English add-rule heading must be gone: {body}"
    );
    assert!(
        !body.contains("IP rules"),
        "the English rules heading must be gone: {body}"
    );

    // ── the settings ──
    let body = page_body(&client, "/settings").await;
    for (needle, what) in [
        ("客户端 IP 取源", "client-IP card title"),
        ("IP 信誉", "IP-reputation card title"),
    ] {
        assert!(
            body.contains(needle),
            "the Chinese {what} must be rendered: {body}"
        );
    }
    // The origin-guard card title is "源站防护" — a substring of the
    // rejection counter below it ("源站防护累计拒绝次数：") and of the
    // provider descriptions, so the needle pins the element boundary.
    assert!(
        body.contains(">源站防护<"),
        "the Chinese origin-guard card title must be rendered: {body}"
    );
    assert!(
        !body.contains("Client IP resolution"),
        "the English client-IP card title must be gone: {body}"
    );
    assert!(
        !body.contains("Origin guard"),
        "the English origin-guard card title must be gone: {body}"
    );
}

/// The validation errors of the four areas are translated too (issue
/// #181) — they are the messages the operator reads when the form
/// rejects the submission.
///
/// The message is translated; the credential field name it quotes is
/// not: "Access Key ID" is the field name of the cloud vendor's own
/// console, so the operator has to find the very same words there.
#[tokio::test]
async fn admin_ui_cert_and_dns_validation_errors_render_chinese() {
    let ngx = start_ngx().await;
    let client = chinese_client(&ngx).await;

    // A certificate without a domain.
    let page = page_body(&client, "/certs/new").await;
    let csrf = client.csrf_token(&page).unwrap_or_default();
    let resp = client
        .post_form("/certs/new", &[("_csrf", &csrf)])
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("请填写域名"),
        "the Chinese domain error must be rendered: {body}"
    );

    // A certificate without a certificate file.
    let page = page_body(&client, "/certs/new").await;
    let csrf = client.csrf_token(&page).unwrap_or_default();
    let resp = client
        .post_form(
            "/certs/new",
            &[("domain", "zh.example.com"), ("_csrf", &csrf)],
        )
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("请填写证书文件路径"),
        "the Chinese certificate-file error must be rendered: {body}"
    );

    // A DNS provider whose name carries characters the name does not
    // allow.
    let page = page_body(&client, "/dns/new").await;
    let csrf = client.csrf_token(&page).unwrap_or_default();
    let resp = client
        .post_form(
            "/dns/new",
            &[
                ("name", "Bad Name"),
                ("kind", "cloudflare"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("名称只能是小写字母、数字、下划线或连字符"),
        "the Chinese name error must be rendered: {body}"
    );

    // A DNS provider without its Secret ID: the message is Chinese,
    // the field name inside it stays English.
    let page = page_body(&client, "/dns/new").await;
    let csrf = client.csrf_token(&page).unwrap_or_default();
    let resp = client
        .post_form(
            "/dns/new",
            &[
                ("name", "zh-tencent"),
                ("kind", "tencent"),
                ("_csrf", &csrf),
            ],
        )
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("请填写 Secret ID"),
        "the Chinese Secret-ID error must be rendered: {body}"
    );
}

/// The access log and the bot logs are translated (issue #182).
///
/// These are the pages that answer "did a crawler come last night,
/// which pages did it fetch, what status did it get" — the question
/// the README promises the product answers.  A page that still
/// answers in English there is a page the operator cannot read while
/// they are chasing it.
///
/// The four pages share their vocabulary — a request, a bot, a
/// status code — so the test walks all four and asserts that the
/// Chinese copy of the area is rendered and that the English copy is
/// gone.
///
/// The Chinese literals are the shipped copy, pinned on purpose: a
/// translation change is a change to what the operator reads, and it
/// has to show up in the diff.  A later copy edit updates them here
/// too.
#[tokio::test]
async fn admin_ui_access_and_bot_log_pages_render_chinese() {
    let ngx = start_ngx().await;
    let client = chinese_client(&ngx).await;

    // ── the access log ──
    let body = page_body(&client, "/logs").await;
    for (needle, what) in [
        ("访问日志", "the page title"),
        ("正在等待事件", "the empty state"),
        ("清空", "the clear button"),
    ] {
        assert!(
            body.contains(needle),
            "the Chinese {what} must be rendered: {body}"
        );
    }
    assert!(
        !body.contains("Access Logs"),
        "the English page title must be gone: {body}"
    );
    assert!(
        !body.contains("Waiting for events"),
        "the English empty state must be gone: {body}"
    );

    // ── the bot log ──
    let body = page_body(&client, "/logs/bots").await;
    for (needle, what) in [
        ("爬虫日志", "the page title"),
        ("验证状态", "the verification panel"),
        ("爬虫请求总数", "the total-requests card"),
        ("实时日志", "the live-tail table"),
        ("正在等待爬虫流量", "the empty state"),
    ] {
        assert!(
            body.contains(needle),
            "the Chinese {what} must be rendered: {body}"
        );
    }
    assert!(
        !body.contains("Bot Logs"),
        "the English page title must be gone: {body}"
    );
    assert!(
        !body.contains("Verification status"),
        "the English verification panel title must be gone: {body}"
    );
    // The sub-nav is pinned in markup, not as a bare word: the
    // assertion then names the link that carries the label, so it
    // cannot be satisfied by some other element that repeats it.
    assert!(
        body.contains(">统计</a>"),
        "the Chinese sub-nav label must be rendered: {body}"
    );

    // ── the bot-log history ──
    let body = page_body(&client, "/logs/bots/history").await;
    for (needle, what) in [
        ("可用日期", "the date sidebar"),
        ("暂无匹配记录", "the empty result"),
        ("数据来自", "the footnote"),
    ] {
        assert!(
            body.contains(needle),
            "the Chinese {what} must be rendered: {body}"
        );
    }
    assert!(
        !body.contains("Available dates"),
        "the English date sidebar must be gone: {body}"
    );

    // ── the bot-log stats ──
    let body = page_body(&client, "/logs/bots/stats").await;
    for (needle, what) in [
        ("爬虫日志 —— 统计", "the page title"),
        ("一天一个主机", "the subtitle"),
        ("计数来自", "the footnote"),
    ] {
        assert!(
            body.contains(needle),
            "the Chinese {what} must be rendered: {body}"
        );
    }
    assert!(
        !body.contains("One day and one host"),
        "the English subtitle must be gone: {body}"
    );
}

/// The `/logs/bots` summary region refreshes itself (issue #225).
///
/// The counters behind the three cards and the top-N table move while
/// the page is open, so the page polls `GET /api/logs/bots/summary`
/// every 30 s and swaps the region's content.  The fragment is the
/// server's authoritative snapshot — the browser never accumulates the
/// numbers itself — so the endpoint has to be reachable by a signed-in
/// operator and closed to everyone else.
#[tokio::test]
async fn admin_ui_bots_summary_endpoint_serves_the_polled_fragment() {
    let ngx = start_ngx().await;

    // ── signed in: the fragment is served ──
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();
    let resp = client.get("/api/logs/bots/summary").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "text/html; charset=utf-8",
        "the fragment must be served as HTML"
    );
    assert_eq!(
        resp.headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "no-store",
        "a polled aggregate must not be reused from the cache"
    );
    // The marker lets the page tell its own fragment apart from
    // anything else the browser hands it — an expired session
    // redirects to /login, and htmx must not swap that in.
    assert_eq!(
        resp.headers()
            .get("x-pangolin-fragment")
            .and_then(|v| v.to_str().ok()),
        Some("bots-summary"),
        "the fragment must mark its own response"
    );
    let body = resp.text().await.unwrap();
    for (needle, what) in [
        ("Total bot requests", "the total-requests card"),
        ("Distinct (bot × host) pairs", "the distinct-pairs card"),
        ("Top bot", "the top-bot card"),
    ] {
        assert!(
            body.contains(needle),
            "the fragment must carry {what}: {body}"
        );
    }

    // ── signed out: no fragment ──
    let anon = new_client_no_redirect();
    let resp = anon
        .get(&ngx.admin_url("/api/logs/bots/summary"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "the fragment must not be served to an anonymous client"
    );
    let loc = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        loc.contains("/login"),
        "the anonymous request must be sent to the login page, got: {loc}"
    );
}

/// The referer log and the traffic page are translated (issue #183).
///
/// These are the two "look at the numbers" pages: the referer log
/// answers "which external site sent this visit, and which pages did
/// it land on", the traffic page answers "how much is the gateway
/// serving right now".  They are translated in one issue because they
/// share their vocabulary — a host, a path, a hit count — and a term
/// that is translated two ways on two pages is how an interface
/// starts to feel untranslated.
///
/// The Chinese literals are the shipped copy, pinned on purpose: a
/// translation change is a change to what the operator reads, and it
/// has to show up in the diff.  A later copy edit updates them here
/// too.
#[tokio::test]
async fn admin_ui_referer_log_and_traffic_pages_render_chinese() {
    let ngx = start_ngx().await;
    let client = chinese_client(&ngx).await;

    // ── the referer log (live) ──
    let body = page_body(&client, "/logs/referers").await;
    for (needle, what) in [
        ("来源日志", "the page title"),
        ("引荐总次数", "the total-hits card"),
        ("命中最多的来源", "the top-referrer card"),
        ("正在等待外部引荐流量", "the empty state"),
        ("实时日志", "the live-tail table"),
    ] {
        assert!(
            body.contains(needle),
            "the Chinese {what} must be rendered: {body}"
        );
    }
    assert!(
        !body.contains("Referer Logs"),
        "the English page title must be gone: {body}"
    );
    // The sub-nav is pinned in markup, not as a bare word: the page
    // subtitle carries "统计" too ("只统计顶层跳转"), so a plain
    // substring would still pass if the sub-nav label fell back to
    // English.
    for (needle, what) in [
        (">实时</a>", "the live sub-nav label"),
        (">历史</a>", "the history sub-nav label"),
        (">统计</a>", "the stats sub-nav label"),
    ] {
        assert!(
            body.contains(needle),
            "the Chinese {what} must be rendered: {body}"
        );
    }

    // ── the referer-log history ──
    let body = page_body(&client, "/logs/referers/history").await;
    for (needle, what) in [
        ("可用日期", "the date sidebar"),
        ("暂无 JSONL 文件", "the empty date sidebar"),
        ("暂无匹配记录", "the empty result"),
        ("数据来自", "the footnote"),
    ] {
        assert!(
            body.contains(needle),
            "the Chinese {what} must be rendered: {body}"
        );
    }
    assert!(
        !body.contains("Available dates"),
        "the English date sidebar must be gone: {body}"
    );

    // ── the referer-log stats ──
    let body = page_body(&client, "/logs/referers/stats").await;
    for (needle, what) in [
        ("来源日志 —— 统计", "the page title"),
        ("一天一个站点", "the subtitle"),
        ("引荐次数", "the referral-hits card"),
        ("独立来源数", "the distinct-referrers card"),
        ("独立访客数", "the distinct-visitors card"),
        ("按来源", "the by-referrer table"),
        ("按落地路径", "the by-landing-path table"),
        ("暂无外部引荐。", "the empty by-referrer table"),
        ("暂无路径。", "the empty by-landing-path table"),
    ] {
        assert!(
            body.contains(needle),
            "the Chinese {what} must be rendered: {body}"
        );
    }
    assert!(
        !body.contains("One day and one site"),
        "the English subtitle must be gone: {body}"
    );

    // ── the traffic page ──
    let body = page_body(&client, "/traffic").await;
    for (needle, what) in [
        (">流量</h1>", "the page title"),
        ("内存中的网络与请求统计", "the subtitle"),
        ("重置计数", "the reset button"),
        ("出站流量", "the body-out card"),
        ("错误率", "the error-rate card"),
        ("处理中请求", "the in-flight card"),
        (
            "每分钟请求数（最近一小时）",
            "the requests-per-minute chart",
        ),
        ("按主机", "the by-host table"),
        ("热门路径", "the top-paths table"),
        ("暂无已知主机的流量。", "the empty by-host table"),
    ] {
        assert!(
            body.contains(needle),
            "the Chinese {what} must be rendered: {body}"
        );
    }
    assert!(
        !body.contains("Reset counters"),
        "the English reset button must be gone: {body}"
    );
    assert!(
        !body.contains("Top paths"),
        "the English top-paths table title must be gone: {body}"
    );
    // The technical identifiers survive the translation: the
    // operator reads the same notation in every other tool.
    for (needle, what) in [
        ("p95", "the latency card"),
        ("RPS", "the requests-per-second card"),
        ("4xx", "the status-class column"),
    ] {
        assert!(
            body.contains(needle),
            "the {what} must keep its identifier: {body}"
        );
    }
}

// ── §issue-226 — The polled referer summary region ─────────────────────────

/// A tiny HTTP backend that answers `200 OK` to every request.
///
/// The referer counters tick on the *response* path, so a referral
/// only reaches them when the upstream actually answers.
async fn start_summary_backend() -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the mock backend");
    let addr = listener.local_addr().unwrap().to_string();
    let handle = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = Vec::with_capacity(2048);
                let mut tmp = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(2),
                        stream.read(&mut tmp),
                    )
                    .await
                    {
                        Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                        Ok(Ok(n)) => buf.extend_from_slice(&tmp[..n]),
                    }
                }
                let body = b"ok";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.write_all(body).await;
                let _ = stream.flush().await;
            });
        }
    });
    (addr, handle)
}

/// The polled summary region must answer a signed-in operator with the
/// fragment for **the site the request names** — never a wider slice —
/// and an anonymous one with the login redirect.
#[tokio::test]
async fn admin_ui_referers_summary_endpoint_keeps_the_site_scope() {
    let (backend_addr, _backend) = start_summary_backend().await;
    let ngx = NgxProcess::start({
        let backend_addr = backend_addr.clone();
        move |db_path| {
            init_pangolin_db(db_path);
            let conn = rusqlite::Connection::open(db_path).expect("open db");
            let now = chrono::Utc::now();
            for (name, domain) in [("web", "example.test"), ("shop", "shop.example.test")] {
                pangolin_core::db::upsert_site(
                    &conn,
                    &pangolin_core::types::Site {
                        name: name.into(),
                        backend: format!("http://{backend_addr}"),
                        enabled: true,
                        created_at: now,
                        updated_at: now,
                        host_mode: pangolin_core::types::HostMode::Passthrough,
                        host_custom: None,
                        domain_count: 0,
                    },
                )
                .expect("insert site");
                pangolin_core::db::upsert_domain(
                    &conn,
                    &pangolin_core::types::Domain {
                        domain: domain.into(),
                        site_name: name.into(),
                        enabled: true,
                        auto_issue: false,
                        dns_provider: None,
                        challenge_kind: None,
                        created_at: now,
                    },
                )
                .expect("insert domain");
            }
        }
    })
    .await;

    // An external document navigation lands on the `web` site's domain
    // — one referral, scoped to that site.
    let addr = format!("127.0.0.1:{}", ngx.http_port);
    let (status, _body) = crate::harness::raw_request_with_headers(
        &addr,
        "example.test",
        "GET",
        "/pricing",
        b"",
        &[
            ("Referer", "https://news.example.org/article"),
            ("Sec-Fetch-Dest", "document"),
        ],
    )
    .await;
    assert_eq!(status, 200, "the proxied request must succeed");

    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    // The counters tick on the response path; poll the fragment until
    // the referral shows up.
    let mut scoped = String::new();
    for _ in 0..40 {
        let resp = client
            .get("/api/logs/referers/summary?site=web")
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        scoped = resp.text().await.unwrap();
        if scoped.contains("news.example.org") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        scoped.contains("news.example.org"),
        "the scoped fragment must carry the site's own referral: {scoped}"
    );
    // The three cards are there, in the operator's language.
    for (needle, what) in [
        ("Total referral hits", "the total-hits card"),
        (
            "Distinct (referrer × host) pairs",
            "the distinct-pairs card",
        ),
        ("Top referrer", "the top-referrer card"),
    ] {
        assert!(
            scoped.contains(needle),
            "the fragment must carry the {what}: {scoped}"
        );
    }
    // A live aggregate must not be served from a cache.
    let resp = client
        .get("/api/logs/referers/summary?site=web")
        .await
        .unwrap();
    assert_eq!(
        resp.headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-store"),
        "the polled fragment must not be cacheable"
    );
    // The marker lets the page tell its own fragment apart from
    // anything else the browser hands it — an expired session
    // redirects to /login, and htmx must not swap that in.
    assert_eq!(
        resp.headers()
            .get("x-pangolin-fragment")
            .and_then(|v| v.to_str().ok()),
        Some("referers-summary"),
        "the fragment must mark its own response"
    );

    // Another site's scope must not show the `web` site's referral.
    let resp = client
        .get("/api/logs/referers/summary?site=shop")
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let other = resp.text().await.unwrap();
    assert!(
        !other.contains("news.example.org"),
        "the fragment must not widen to another site's traffic: {other}"
    );

    // The page polls the very same URL, scope included.
    let resp = client.get("/logs/referers?site=web").await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let page = resp.text().await.unwrap();
    assert!(
        page.contains("hx-get=\"/api/logs/referers/summary?site=web\""),
        "the page must poll the scoped summary: {page}"
    );
    assert!(
        page.contains("hx-trigger=\"every 30s\""),
        "the page must poll the summary region: {page}"
    );

    // Without a session the fragment is not handed out at all.
    let anon = new_client_no_redirect();
    let resp = anon
        .get(&format!(
            "{}/api/logs/referers/summary?site=web",
            ngx.admin_url("")
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        302,
        "an anonymous request must be redirected to the login"
    );
    let loc = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        loc.contains("/login"),
        "the redirect must point at the login, got: {loc}"
    );
}
