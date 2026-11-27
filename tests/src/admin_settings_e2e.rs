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
    // The list-refresh-interval setting is gone (issue #151): the
    // library owns the refresh cadence now, so the page offers no
    // interval — it only tells the operator where the cadence comes
    // from.
    assert!(
        !body.contains("badbots_feed_refresh_interval_secs"),
        "the removed setting must not be on the page"
    );
    assert!(
        !body.contains("List refresh interval"),
        "the removed setting must not be on the page"
    );
    assert!(
        body.contains("at most once a day"),
        "the page must say where the refresh cadence comes from: {body}"
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

// ── The dashboard card (issue #185, reworked by issue #262) ───────────────
//
// The dashboard channel's settings live in the database, not in
// `ngx.yml`: the operator sets the bearer token and the dash API from
// this card, and the change takes effect without a restart.

/// The settings page shows the dashboard card, with the instance id,
/// the dash API and the key fingerprint — and never the token.
#[tokio::test]
async fn settings_page_renders_the_dashboard_card() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let body = client.get("/settings").await.unwrap().text().await.unwrap();
    assert!(
        body.contains(r#"action="/settings/dashboard""#),
        "the dashboard form must be present"
    );
    for field in [
        "dashboard_dash_api",
        "dashboard_token",
        "dashboard_token_clear",
    ] {
        assert!(
            body.contains(&format!(r#"name="{field}""#)),
            "{field} input missing"
        );
    }
    // The instance id is shown — it is how the dashboard knows the
    // node.
    assert!(
        body.contains(r#"id="dashboard-instance-id""#),
        "the instance id must be shown"
    );
    // The fingerprint of the key the node trusts is shown too.
    assert!(
        body.contains(r#"id="dashboard-key-fingerprint""#),
        "the key fingerprint must be shown"
    );
    // The token is write-only: the stored token never comes back.
    assert!(
        !body.contains("dashboard_token_clear\" value=\"true\" checked"),
        "the token must not be echoed"
    );
}

/// A token that cannot travel in a request header is refused.
///
/// The channel puts the token in the `Authorization` header of every
/// request: a token with a space or a non-ASCII character would kill
/// the channel on its first request, so it is refused at the door.
#[tokio::test]
async fn settings_post_dashboard_refuses_a_token_that_cannot_travel_in_a_header() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let csrf = client
        .csrf_token(&client.get("/settings").await.unwrap().text().await.unwrap())
        .expect("csrf on /settings");

    let resp = client
        .post_form(
            "/settings/dashboard",
            &[
                ("_csrf", &csrf),
                ("dashboard_token", "the token with a space"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "the page is re-rendered");
    let body = resp.text().await.unwrap();
    assert!(
        body.contains(r#"data-testid="dashboard-error""#),
        "the error must be shown in the dashboard card: {body}"
    );

    // Nothing was written.
    let conn = rusqlite::Connection::open(ngx.db_path()).expect("open db");
    let token: String = conn
        .query_row("SELECT token FROM api_config WHERE id = 1", [], |r| {
            r.get(0)
        })
        .expect("the row exists");
    assert_eq!(token, "", "a refused form must not write");
}

/// A plain-HTTP dashboard address that leaves the machine is refused.
///
/// The channel carries the token in every request, so a plain-HTTP
/// address would put the credential on the wire in clear text.
#[tokio::test]
async fn settings_post_dashboard_refuses_an_insecure_dashboard_address() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let csrf = client
        .csrf_token(&client.get("/settings").await.unwrap().text().await.unwrap())
        .expect("csrf on /settings");

    let resp = client
        .post_form(
            "/settings/dashboard",
            &[
                ("_csrf", &csrf),
                ("dashboard_dash_api", "http://dash.example.com/api/v1/"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "the page is re-rendered");
    let body = resp.text().await.unwrap();
    assert!(
        body.contains(r#"data-testid="dashboard-error""#),
        "the error must be shown in the dashboard card: {body}"
    );

    // Nothing was written.
    let conn = rusqlite::Connection::open(ngx.db_path()).expect("open db");
    let dash_api: String = conn
        .query_row("SELECT dash_api FROM api_config WHERE id = 1", [], |r| {
            r.get(0)
        })
        .expect("the row exists");
    assert_eq!(dash_api, "", "a refused form must not write");
}

/// The dashboard form writes the row — the token in the clear (the
/// node signs with it) and the dash API as it was typed.
#[tokio::test]
async fn settings_post_dashboard_saves_the_row_without_echoing_the_token() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let csrf = client
        .csrf_token(&client.get("/settings").await.unwrap().text().await.unwrap())
        .expect("csrf on /settings");

    let resp = client
        .post_form(
            "/settings/dashboard",
            &[
                ("_csrf", &csrf),
                ("dashboard_token", "the-operator-token"),
                ("dashboard_dash_api", "https://dash.example.com/api/v1/"),
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
    let (token, dash_api): (String, String) = conn
        .query_row(
            "SELECT token, dash_api FROM api_config WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("the row exists");
    assert_eq!(token, "the-operator-token", "the token must be stored");
    assert_eq!(
        dash_api, "https://dash.example.com/api/v1/",
        "the dash API must be stored"
    );

    // And the page never renders it back.
    let body = client.get("/settings").await.unwrap().text().await.unwrap();
    assert!(
        !body.contains("the-operator-token"),
        "the token must not be echoed back"
    );
}

/// An empty token field keeps the stored token.
#[tokio::test]
async fn settings_post_dashboard_keeps_the_token_when_the_field_is_empty() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let csrf = client
        .csrf_token(&client.get("/settings").await.unwrap().text().await.unwrap())
        .expect("csrf on /settings");

    client
        .post_form(
            "/settings/dashboard",
            &[
                ("_csrf", &csrf),
                ("dashboard_token", "the-operator-token"),
                ("dashboard_dash_api", "https://dash.example.com/api/v1/"),
            ],
        )
        .await
        .unwrap();

    // A second save with an empty token field keeps the stored one.
    let csrf = client
        .csrf_token(&client.get("/settings").await.unwrap().text().await.unwrap())
        .expect("csrf on /settings");
    let resp = client
        .post_form(
            "/settings/dashboard",
            &[
                ("_csrf", &csrf),
                ("dashboard_token", ""),
                ("dashboard_dash_api", "https://dash.example.com/api/v1/"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 302);

    let conn = rusqlite::Connection::open(ngx.db_path()).expect("open db");
    let token: String = conn
        .query_row("SELECT token FROM api_config WHERE id = 1", [], |r| {
            r.get(0)
        })
        .expect("the row exists");
    assert_eq!(
        token, "the-operator-token",
        "an empty token field must keep the stored token"
    );
}

/// The "remove" checkbox clears the token.
#[tokio::test]
async fn settings_post_dashboard_clears_the_token_on_request() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let csrf = client
        .csrf_token(&client.get("/settings").await.unwrap().text().await.unwrap())
        .expect("csrf on /settings");
    client
        .post_form(
            "/settings/dashboard",
            &[("_csrf", &csrf), ("dashboard_token", "the-operator-token")],
        )
        .await
        .unwrap();

    let csrf = client
        .csrf_token(&client.get("/settings").await.unwrap().text().await.unwrap())
        .expect("csrf on /settings");
    let resp = client
        .post_form(
            "/settings/dashboard",
            &[("_csrf", &csrf), ("dashboard_token_clear", "true")],
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 302);

    let conn = rusqlite::Connection::open(ngx.db_path()).expect("open db");
    let token: String = conn
        .query_row("SELECT token FROM api_config WHERE id = 1", [], |r| {
            r.get(0)
        })
        .expect("the row exists");
    assert_eq!(token, "", "the token must be cleared");
}

/// An empty dash API field puts the built-in default back in force.
#[tokio::test]
async fn settings_post_dashboard_falls_back_to_the_built_in_dash_api() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let csrf = client
        .csrf_token(&client.get("/settings").await.unwrap().text().await.unwrap())
        .expect("csrf on /settings");
    client
        .post_form(
            "/settings/dashboard",
            &[
                ("_csrf", &csrf),
                ("dashboard_dash_api", "https://dash.example.com/api/v1/"),
            ],
        )
        .await
        .unwrap();

    // The card shows the configured address.
    let body = client.get("/settings").await.unwrap().text().await.unwrap();
    assert!(
        body.contains("https://dash.example.com/api/v1/"),
        "the configured dash API must be shown: {body}"
    );

    // Clearing the field puts the built-in default back in force.
    let csrf = client
        .csrf_token(&client.get("/settings").await.unwrap().text().await.unwrap())
        .expect("csrf on /settings");
    let resp = client
        .post_form(
            "/settings/dashboard",
            &[("_csrf", &csrf), ("dashboard_dash_api", "")],
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 302);

    let conn = rusqlite::Connection::open(ngx.db_path()).expect("open db");
    let dash_api: String = conn
        .query_row("SELECT dash_api FROM api_config WHERE id = 1", [], |r| {
            r.get(0)
        })
        .expect("the row exists");
    assert_eq!(dash_api, "", "the stored dash API must be empty");

    let body = client.get("/settings").await.unwrap().text().await.unwrap();
    assert!(
        body.contains(pangolin_core::db::DEFAULT_DASH_API),
        "the built-in default must be shown: {body}"
    );
}

// ── §8 — The version counters (issue #273) ─────────────────────────────────
//
// The two global singletons carry two version counters: the node's own
// `revision` and the `synced_revision` the dashboard last pushed. The
// settings page shows both, and says that the dashboard's next sync
// overwrites a local change when the two differ.

#[tokio::test]
async fn the_settings_page_shows_the_two_versions_and_the_overwrite_notice() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let body = client.get("/settings").await.unwrap().text().await.unwrap();
    assert!(
        body.contains(r#"data-testid="system-config-revision""#),
        "the page must show the version counters: {body}"
    );
    assert!(
        body.contains(r#"data-testid="badbots-revision""#),
        "the page must show the reputation counters: {body}"
    );
    // A node that never synchronised with the dashboard carries
    // revision 1 and synced_revision 0 — the two differ, so the page
    // warns that the dashboard's next sync overwrites the local
    // configuration.
    assert!(
        body.contains(r#"data-testid="system-config-drift""#),
        "a node that never synchronised must warn the operator: {body}"
    );
    assert!(
        body.contains(r#"data-testid="badbots-drift""#),
        "the reputation card must warn the operator too: {body}"
    );
}

#[tokio::test]
async fn a_synchronised_configuration_shows_no_overwrite_notice() {
    // The row is seeded as the dashboard left it: the two counters
    // are equal, so nothing is going to be overwritten.
    let ngx = NgxProcess::start(|db_path| {
        init_pangolin_db(db_path);
        let conn = rusqlite::Connection::open(db_path).expect("open db");
        conn.execute(
            "UPDATE system_config SET revision = 3, synced_revision = 3 WHERE id = 1",
            [],
        )
        .expect("seed the counters");
        conn.execute(
            "UPDATE badbots_config SET revision = 2, synced_revision = 2 WHERE id = 1",
            [],
        )
        .expect("seed the counters");
    })
    .await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let body = client.get("/settings").await.unwrap().text().await.unwrap();
    assert!(
        body.contains(r#"data-testid="system-config-revision""#),
        "the page must still show the counters: {body}"
    );
    assert!(
        !body.contains(r#"data-testid="system-config-drift""#),
        "a synchronised configuration must not warn about an overwrite: {body}"
    );
    assert!(
        !body.contains(r#"data-testid="badbots-drift""#),
        "a synchronised reputation configuration must not warn either: {body}"
    );
}

#[tokio::test]
async fn a_local_change_moves_the_revision_and_keeps_the_drift_notice() {
    let ngx = start_ngx().await;
    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.unwrap();

    let csrf = client
        .csrf_token(&client.get("/settings").await.unwrap().text().await.unwrap())
        .expect("csrf on /settings");

    // The operator changes the provider in the admin UI — a **local**
    // write: the revision moves, the sync counter does not.
    let resp = client
        .post_form(
            "/settings",
            &[
                ("_csrf", &csrf),
                ("provider", "cloudflare"),
                ("origin_guard_enabled", "false"),
                ("pgl_headers_enabled", "true"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 302);

    let conn = rusqlite::Connection::open(ngx.db_path()).expect("open db");
    let (revision, synced_revision): (i64, i64) = conn
        .query_row(
            "SELECT revision, synced_revision FROM system_config WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("the row exists");
    assert_eq!(revision, 2, "the content changed, so the revision moved");
    assert_eq!(
        synced_revision, 0,
        "a local write must not move the sync counter"
    );

    // The page still warns that the dashboard's next sync overwrites
    // the local change.
    let body = client.get("/settings").await.unwrap().text().await.unwrap();
    assert!(
        body.contains(r#"data-testid="system-config-drift""#),
        "a local change must warn the operator: {body}"
    );
}
