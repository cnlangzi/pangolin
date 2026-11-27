//! The dashboard-facing management API — the router's integration suite
//! (issue #185, reworked by issue #262).
//!
//! Since issue #262 the API has **no listener**: the node dials **out**
//! to the dashboard, and the dashboard's commands are executed by
//! [`api::server::route`]. This suite drives the router itself, so the
//! resource shapes, the error envelope and the write semantics are
//! covered without a transport in between.
//!
//! The **channel** — the handshake, the SSE subscription, the report of
//! a result and the reconnection — is covered by [`crate::api_e2e`],
//! which drives a real outbound client against a mock dashboard.

use std::sync::Arc;

use api::server::{ApiRequest, ApiState};
use pangolin_core::{App, CertManager};

/// The instance id the suite's gateway is known by.
const INSTANCE_ID: &str = "test-instance";

/// A running gateway behind the router.
struct ApiServer {
    app: Arc<App>,
    state: ApiState,
    _dir: tempfile::TempDir,
}

impl ApiServer {
    /// Call the router and parse the answer as JSON.
    async fn call(&self, method: &str, path: &str, body: &[u8]) -> (u16, serde_json::Value) {
        let (status, body) = self.call_raw(method, path, body).await;
        let json = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    /// Call the router and keep the answer's bytes.
    async fn call_raw(&self, method: &str, path: &str, body: &[u8]) -> (u16, Vec<u8>) {
        let request = ApiRequest::new(method, path, body.to_vec());
        let response = api::server::route(&self.state, request).await;
        (response.status, response.body)
    }
}

/// Start a gateway behind the router.
async fn start_api() -> ApiServer {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("pangolin.db");
    // The log directories point into the tempdir — the default ones
    // are relative to the working directory, and a test must not write
    // into the repository.
    let mut config = pangolin_core::Config::default();
    config.log.bot.dir = dir.path().join("logs/bots");
    config.log.bot.cache_dir = dir.path().join("logs/bots/cache");
    config.log.referer.dir = dir.path().join("logs/referers");
    let app = Arc::new(
        App::new(
            db_path.to_str().expect("utf8"),
            config,
            CertManager::default(),
        )
        .expect("App::new"),
    );
    // The row the instance view reads.
    {
        let db = app.db.lock().await;
        pangolin_core::db::upsert_api_config(
            &db,
            &pangolin_core::db::ApiConfigRow {
                id: 1,
                instance_id: INSTANCE_ID.into(),
                token: "the-test-token".into(),
                dash_api: "https://dash.example.com/api/v1/".into(),
                updated_at: chrono::Utc::now().to_rfc3339(),
            },
        )
        .expect("seed the api_config row");
    }
    let state = ApiState::new(app.clone(), api::protocol::signature::dashboard_key());
    ApiServer {
        app,
        state,
        _dir: dir,
    }
}

/// The error code of a failed answer.
fn error_code(json: &serde_json::Value) -> String {
    json["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// Create a site.
async fn create_site(server: &ApiServer, name: &str) {
    let body = serde_json::json!({
        "name": name,
        "backend": "http://127.0.0.1:8080",
    })
    .to_string();
    let (status, json) = server.call("POST", "/api/v1/sites", body.as_bytes()).await;
    assert_eq!(status, 201, "{json}");
}

// ── The round trip ──────────────────────────────────────────────────────

#[tokio::test]
async fn a_request_reads_the_sites() {
    let server = start_api().await;
    let (status, json) = server.call("GET", "/api/v1/sites", b"").await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json, serde_json::json!([]));
}

#[tokio::test]
async fn a_site_is_created_read_back_and_deleted() {
    let server = start_api().await;
    let body = serde_json::json!({
        "name": "example",
        "backend": "http://127.0.0.1:8080",
    })
    .to_string();

    let (status, json) = server.call("POST", "/api/v1/sites", body.as_bytes()).await;
    assert_eq!(status, 201, "{json}");
    assert_eq!(json["name"], "example");
    assert_eq!(json["backend"], "http://127.0.0.1:8080");

    let (status, json) = server.call("GET", "/api/v1/sites/example", b"").await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["name"], "example");

    let (status, json) = server.call("DELETE", "/api/v1/sites/example", b"").await;
    assert_eq!(status, 200, "{json}");

    let (status, json) = server.call("GET", "/api/v1/sites/example", b"").await;
    assert_eq!(status, 404, "{json}");
    assert_eq!(error_code(&json), "site_not_found");
}

#[tokio::test]
async fn a_site_that_exists_is_a_conflict() {
    let server = start_api().await;
    let body = serde_json::json!({
        "name": "example",
        "backend": "http://127.0.0.1:8080",
    })
    .to_string();
    let (status, _) = server.call("POST", "/api/v1/sites", body.as_bytes()).await;
    assert_eq!(status, 201);
    let (status, json) = server.call("POST", "/api/v1/sites", body.as_bytes()).await;
    assert_eq!(status, 409, "{json}");
    assert_eq!(error_code(&json), "site_exists");
}

#[tokio::test]
async fn a_site_without_a_backend_is_refused() {
    // The admin's own message, verbatim (issue #185, R11).
    let server = start_api().await;
    let body = serde_json::json!({ "name": "example" }).to_string();
    let (status, json) = server.call("POST", "/api/v1/sites", body.as_bytes()).await;
    assert_eq!(status, 422, "{json}");
    assert_eq!(error_code(&json), "backend_invalid");
    assert_eq!(
        json["error"]["message"],
        "Backend is required — fill in the host:port (or file path) field"
    );
    assert_eq!(json["error"]["field"], "backend");
}

// ── The namespace ───────────────────────────────────────────────────────

#[tokio::test]
async fn the_admin_routes_do_not_exist_on_the_api() {
    let server = start_api().await;
    for path in ["/", "/sites", "/settings", "/api/logs/stream"] {
        let (status, json) = server.call("GET", path, b"").await;
        assert_eq!(status, 404, "{path} -> {json}");
    }
}

#[tokio::test]
async fn an_unknown_endpoint_is_a_not_found() {
    let server = start_api().await;
    let (status, json) = server.call("GET", "/api/v1/nope", b"").await;
    assert_eq!(status, 404, "{json}");
}

// ── The credentials are never echoed ────────────────────────────────────

#[tokio::test]
async fn a_dns_provider_never_echoes_its_token() {
    let server = start_api().await;
    let body = serde_json::json!({
        "name": "cf",
        "kind": "cloudflare",
        "api_token": "super-secret",
    })
    .to_string();
    let (status, json) = server
        .call("POST", "/api/v1/dns_providers", body.as_bytes())
        .await;
    assert_eq!(status, 201, "{json}");
    assert_eq!(json["has_api_token"], true);

    let (status, body) = server.call_raw("GET", "/api/v1/dns_providers", b"").await;
    assert_eq!(status, 200);
    let body = String::from_utf8_lossy(&body).into_owned();
    assert!(!body.contains("super-secret"), "{body}");
}

#[tokio::test]
async fn a_tun_never_echoes_its_token_but_returns_it_once() {
    let server = start_api().await;
    let body = serde_json::json!({ "name": "office" }).to_string();
    let (status, json) = server.call("POST", "/api/v1/tun", body.as_bytes()).await;
    assert_eq!(status, 201, "{json}");
    // The token is in the create answer — and nowhere else.
    let token = json["token"].as_str().expect("the token is returned");
    assert!(!token.is_empty());

    let (status, body) = server.call_raw("GET", "/api/v1/tun", b"").await;
    assert_eq!(status, 200);
    let body = String::from_utf8_lossy(&body).into_owned();
    assert!(!body.contains(token), "{body}");
    assert!(body.contains("\"has_token\":true"), "{body}");
}

// ── The write semantics ─────────────────────────────────────────────────

/// The global configuration is **replaced** by a dashboard write
/// (issue #273, R1).
///
/// The body is the desired state: a field it does not mention is
/// cleared or falls back to its default.
#[tokio::test]
async fn the_system_config_is_replaced() {
    let server = start_api().await;
    // Set a value first.
    let body = serde_json::json!({
        "provider": "cloudflare",
        "client_ip_header": "CF-Connecting-IP",
    })
    .to_string();
    let (status, json) = server
        .call("PUT", "/api/v1/system_config", body.as_bytes())
        .await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["provider"], "cloudflare");
    assert_eq!(json["client_ip_header"], "CF-Connecting-IP");

    // A second write that does not mention the header clears it.
    let body = serde_json::json!({ "provider": "cloudflare" }).to_string();
    let (status, json) = server
        .call("PUT", "/api/v1/system_config", body.as_bytes())
        .await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["provider"], "cloudflare");
    assert!(
        json["client_ip_header"].is_null(),
        "the header the body does not mention must be cleared: {json}"
    );
    assert_eq!(json["client_country_header"], serde_json::Value::Null);
    assert_eq!(json["client_region_header"], serde_json::Value::Null);
    assert_eq!(
        json["pgl_headers_enabled"], true,
        "an omitted switch falls back to its default: {json}"
    );
    assert_eq!(
        json["origin_guard_enabled"], false,
        "an omitted switch falls back to its default: {json}"
    );
}

/// The provider has no "unset" member, so a body without one is
/// refused (issue #273, R4).
#[tokio::test]
async fn a_system_config_without_a_provider_is_refused() {
    let server = start_api().await;
    let body = serde_json::json!({ "pgl_headers_enabled": false }).to_string();
    let (status, json) = server
        .call("PUT", "/api/v1/system_config", body.as_bytes())
        .await;
    assert_eq!(status, 422, "{json}");
    assert_eq!(error_code(&json), "system_config_invalid");
}

/// The global origin guard may not be switched on for a provider that
/// publishes no source-IP ranges (issue #273, R6).
#[tokio::test]
async fn a_system_config_that_opens_the_guard_for_a_provider_without_ranges_is_refused() {
    let server = start_api().await;
    let body = serde_json::json!({
        "provider": "direct",
        "origin_guard_enabled": true,
    })
    .to_string();
    let (status, json) = server
        .call("PUT", "/api/v1/system_config", body.as_bytes())
        .await;
    assert_eq!(status, 422, "{json}");
    assert_eq!(error_code(&json), "system_config_invalid");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("origin guard"),
        "the message must be the one the admin form gives: {json}"
    );
}

#[tokio::test]
async fn the_bot_list_is_replaced() {
    let server = start_api().await;
    create_site(&server, "example").await;
    let body = serde_json::json!({ "bots": ["gptbot", "ccbot"] }).to_string();
    let (status, json) = server
        .call(
            "PUT",
            "/api/v1/site_rate_limit_bots/example",
            body.as_bytes(),
        )
        .await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["bots"], serde_json::json!(["ccbot", "gptbot"]));

    // A second write with fewer entries drops the rest.
    let body = serde_json::json!({ "bots": ["gptbot"] }).to_string();
    let (status, json) = server
        .call(
            "PUT",
            "/api/v1/site_rate_limit_bots/example",
            body.as_bytes(),
        )
        .await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["bots"], serde_json::json!(["gptbot"]));
}

#[tokio::test]
async fn the_origin_allow_list_is_replaced() {
    let server = start_api().await;
    create_site(&server, "example").await;
    let body = serde_json::json!({ "cidrs": ["1.2.3.0/24"] }).to_string();
    let (status, json) = server
        .call("PUT", "/api/v1/site_origin_allow/example", body.as_bytes())
        .await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["cidrs"], serde_json::json!(["1.2.3.0/24"]));

    let body = serde_json::json!({ "cidrs": [] }).to_string();
    let (status, json) = server
        .call("PUT", "/api/v1/site_origin_allow/example", body.as_bytes())
        .await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["cidrs"], serde_json::json!([]));
}

#[tokio::test]
async fn a_reputation_key_is_kept_and_can_be_cleared() {
    let server = start_api().await;
    let body = serde_json::json!({ "abuseipdb_api_key": "key-1" }).to_string();
    let (status, json) = server
        .call("PUT", "/api/v1/badbots_config", body.as_bytes())
        .await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["has_abuseipdb_api_key"], true);
    assert_eq!(json["has_crowdsec_api_key"], false);

    // A write that mentions nothing keeps the key — the credential
    // exception of the replace semantics (issue #273, R3).
    let body = serde_json::json!({ "enabled": false }).to_string();
    let (status, json) = server
        .call("PUT", "/api/v1/badbots_config", body.as_bytes())
        .await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["has_abuseipdb_api_key"], true);
    assert_eq!(json["enabled"], false);
    assert_eq!(
        json["evidence_policy"], "lenient",
        "an omitted policy falls back to its default: {json}"
    );

    // `null` clears it.
    let body = serde_json::json!({ "abuseipdb_api_key": null }).to_string();
    let (status, json) = server
        .call("PUT", "/api/v1/badbots_config", body.as_bytes())
        .await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["has_abuseipdb_api_key"], false);
}

// ── The version counters ────────────────────────────────────────────────

/// The two counters travel with every answer (issue #273, R11).
#[tokio::test]
async fn the_system_config_reports_its_two_versions() {
    let server = start_api().await;
    let (status, json) = server.call("GET", "/api/v1/system_config", b"").await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["revision"], 1, "{json}");
    assert_eq!(json["synced_revision"], 0, "{json}");

    let body = serde_json::json!({ "provider": "cloudflare" }).to_string();
    let (status, json) = server
        .call("PUT", "/api/v1/system_config", body.as_bytes())
        .await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["revision"], 2, "{json}");
    assert_eq!(
        json["synced_revision"], 2,
        "a dashboard write marks the row as synchronised: {json}"
    );

    let (status, json) = server.call("GET", "/api/v1/system_config", b"").await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["revision"], 2, "{json}");
    assert_eq!(json["synced_revision"], 2, "{json}");
}

/// A write that changes nothing does not move the revision
/// (issue #273, R7).
#[tokio::test]
async fn a_dashboard_write_that_changes_nothing_leaves_the_revision_alone() {
    let server = start_api().await;
    let body = serde_json::json!({ "provider": "cloudflare" }).to_string();
    let (status, json) = server
        .call("PUT", "/api/v1/system_config", body.as_bytes())
        .await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["revision"], 2, "{json}");

    // The same content again.
    let (status, json) = server
        .call("PUT", "/api/v1/system_config", body.as_bytes())
        .await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(
        json["revision"], 2,
        "the content did not change, so the revision must not move: {json}"
    );
    assert_eq!(json["synced_revision"], 2, "{json}");
}

/// A local write makes the row drift: the revision moves, the sync
/// counter does not (issue #273, R9).
#[tokio::test]
async fn a_local_write_leaves_the_row_drifted() {
    let server = start_api().await;
    let body = serde_json::json!({ "provider": "cloudflare" }).to_string();
    let (status, json) = server
        .call("PUT", "/api/v1/system_config", body.as_bytes())
        .await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["revision"], 2, "{json}");

    // The operator changes the configuration in the admin UI.
    let mut local = server
        .app
        .with_db_res(pangolin_core::db::get_system_config)
        .await
        .expect("the row exists");
    local.provider = pangolin_core::ProviderKey::CustomLb;
    server
        .app
        .with_db_res(move |db| {
            pangolin_core::db::update_system_config(
                db,
                &local,
                pangolin_core::ConfigWriteSource::Admin,
            )
        })
        .await
        .expect("the local write lands");
    server.app.reload_indexes().await;

    let (status, json) = server.call("GET", "/api/v1/system_config", b"").await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["revision"], 3, "{json}");
    assert_eq!(
        json["synced_revision"], 2,
        "a local write must not move the sync counter: {json}"
    );
    assert_ne!(json["revision"], json["synced_revision"]);
}

/// The reputation configuration carries the same counters
/// (issue #273, R11).
#[tokio::test]
async fn the_reputation_config_reports_its_two_versions() {
    let server = start_api().await;
    let (status, json) = server.call("GET", "/api/v1/badbots_config", b"").await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["revision"], 1, "{json}");
    assert_eq!(json["synced_revision"], 0, "{json}");

    let body = serde_json::json!({ "enabled": false }).to_string();
    let (status, json) = server
        .call("PUT", "/api/v1/badbots_config", body.as_bytes())
        .await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["revision"], 2, "{json}");
    assert_eq!(json["synced_revision"], 2, "{json}");
}

#[tokio::test]
async fn an_unknown_field_is_refused() {
    let server = start_api().await;
    let body = serde_json::json!({
        "name": "example",
        "backend": "http://127.0.0.1:8080",
        "typo": true,
    })
    .to_string();
    let (status, json) = server.call("POST", "/api/v1/sites", body.as_bytes()).await;
    assert_eq!(status, 400, "{json}");
    assert_eq!(error_code(&json), "malformed_request");
}

// ── The instance and the logs ───────────────────────────────────────────

#[tokio::test]
async fn the_instance_describes_the_node() {
    let server = start_api().await;
    let (status, json) = server.call("GET", "/api/v1/instance", b"").await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["instance_id"], INSTANCE_ID);
    assert_eq!(json["version"], pangolin_core::VERSION);
    assert!(json["uptime_seconds"].is_number(), "{json}");
    assert_eq!(
        json["dashboard_key_fingerprint"],
        api::protocol::signature::fingerprint(&api::protocol::signature::embedded_dashboard_key())
    );
    // The node dials **out**, so the address the dashboard has to know
    // is the dash API (issue #262, R14).
    assert_eq!(json["dash_api"], "https://dash.example.com/api/v1/");
    assert!(
        json.get("dashboard").is_none(),
        "the dashboard address is gone: {json}"
    );
    assert!(json["capabilities"].is_array(), "{json}");
}

#[tokio::test]
async fn the_instance_reports_the_built_in_dash_api_when_none_is_configured() {
    let server = start_api().await;
    {
        let db = server.app.db.lock().await;
        let mut row = pangolin_core::db::get_api_config(&db)
            .expect("the row")
            .expect("the row");
        row.dash_api = String::new();
        pangolin_core::db::upsert_api_config(&db, &row).expect("write");
    }
    let (status, json) = server.call("GET", "/api/v1/instance", b"").await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["dash_api"], pangolin_core::db::DEFAULT_DASH_API);
}

#[tokio::test]
async fn the_raw_logs_are_returned_byte_for_byte() {
    let server = start_api().await;
    let dir = server.app.config.log.bot.dir.clone();
    std::fs::create_dir_all(&dir).expect("mkdir the bot log dir");
    let date = chrono::Utc::now().date_naive();
    let path = pangolin_core::bot_log::file_path_for(&dir, date);
    let content = b"{\"line\":1}\n{\"line\":2}\n";
    std::fs::write(&path, content).expect("write the log file");

    let (status, json) = server.call("GET", "/api/v1/logs/bot/dates", b"").await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(
        json,
        serde_json::json!([date.format("%Y-%m-%d").to_string()])
    );

    let path = format!("/api/v1/logs/bot/raw?date={}", date.format("%Y-%m-%d"));
    let (status, body) = server.call_raw("GET", &path, b"").await;
    assert_eq!(status, 200);
    assert_eq!(body, content);
}

#[tokio::test]
async fn a_log_date_that_is_not_a_date_is_refused() {
    let server = start_api().await;
    let (status, json) = server
        .call("GET", "/api/v1/logs/bot/raw?date=yesterday", b"")
        .await;
    assert_eq!(status, 422, "{json}");
    assert_eq!(error_code(&json), "date_invalid");
}

#[tokio::test]
async fn the_access_log_is_empty_when_nothing_happened() {
    let server = start_api().await;
    let (status, json) = server.call("GET", "/api/v1/logs/access", b"").await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json, serde_json::json!([]));
}

// ── The guards the review found ─────────────────────────────────────────

#[tokio::test]
async fn a_rotated_token_is_the_one_the_tunnel_uses() {
    // The stored digest has to follow the new token: `upsert_tun`
    // prefers the digest it already has, so a rotation that does not
    // drop it leaves the tunnel on the old credential.
    let server = start_api().await;
    let body = serde_json::json!({ "name": "office" }).to_string();
    let (status, json) = server.call("POST", "/api/v1/tun", body.as_bytes()).await;
    assert_eq!(status, 201, "{json}");
    let old_token = json["token"]
        .as_str()
        .expect("the token is returned")
        .to_string();

    let body = serde_json::json!({ "token": "the-new-token" }).to_string();
    let (status, json) = server
        .call("PUT", "/api/v1/tun/office", body.as_bytes())
        .await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["token"], "the-new-token");

    let db = server.app.db.lock().await;
    assert!(
        pangolin_core::db::auth_tun(&db, "office", "the-new-token")
            .expect("the lookup")
            .is_some(),
        "the rotated token has to be the stored one"
    );
    assert!(
        pangolin_core::db::auth_tun(&db, "office", &old_token)
            .expect("the lookup")
            .is_none(),
        "the old token has to stop working"
    );
}

#[tokio::test]
async fn an_ip_rule_is_deleted_by_its_natural_key() {
    // The id is `<scope>|<kind>|<cidr>` and a CIDR contains a `/`, so
    // the id is the rest of the path.
    let server = start_api().await;
    let body = serde_json::json!({
        "name": "example",
        "backend": "http://127.0.0.1:8080",
    })
    .to_string();
    let (status, _) = server.call("POST", "/api/v1/sites", body.as_bytes()).await;
    assert_eq!(status, 201);

    let body = serde_json::json!({
        "scope": "example",
        "kind": "blacklist",
        "cidr": "10.0.0.0/8",
    })
    .to_string();
    let (status, json) = server
        .call("POST", "/api/v1/ip_rules", body.as_bytes())
        .await;
    assert_eq!(status, 201, "{json}");
    assert_eq!(json["id"], "example|blacklist|10.0.0.0/8");

    let (status, json) = server
        .call(
            "DELETE",
            "/api/v1/ip_rules/example|blacklist|10.0.0.0/8",
            b"",
        )
        .await;
    assert_eq!(status, 200, "{json}");

    let (status, json) = server.call("GET", "/api/v1/ip_rules", b"").await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json, serde_json::json!([]));
}

#[tokio::test]
async fn a_per_site_write_for_a_site_that_does_not_exist_is_refused() {
    // The sub-resource tables carry no foreign key, so an orphan row
    // would attach itself to a site of that name created later.
    let server = start_api().await;
    let body = serde_json::json!({ "enabled": true }).to_string();
    let (status, json) = server
        .call("PUT", "/api/v1/site_rate_limits/nope", body.as_bytes())
        .await;
    assert_eq!(status, 404, "{json}");
    assert_eq!(error_code(&json), "site_not_found");
}
