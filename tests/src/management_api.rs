//! The dashboard-facing management API — the integration suite
//! (issue #185).
//!
//! The API server runs **in process** over a real TCP port, so the
//! tests exercise the whole path: the HTTP listener, the
//! authentication gate, the router and the database writes.
//!
//! The signing key is the test suite's own — the API crate takes the
//! verifying key as a parameter, so the tests do not need the
//! production key. The end-to-end suite
//! ([`crate::api_e2e`]) covers the real binary and therefore the
//! **embedded** key.

use std::sync::Arc;
use std::time::Duration;

use api::client::{ApiClient, SignedRequest};
use pangolin_core::{App, CertManager};
use tokio_util::sync::CancellationToken;

/// The token the tests authenticate with.
const TOKEN: &str = "the-test-token";

/// The signing seed of the tests — a key pair of their own.
fn test_seed() -> [u8; 32] {
    let mut seed = [0u8; 32];
    for (i, b) in seed.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(7).wrapping_add(3);
    }
    seed
}

/// The public key of [`test_seed`].
fn test_key() -> [u8; 32] {
    api::protocol::signature::public_key_from_seed(&test_seed()).expect("the seed yields a key")
}

/// A running API server plus the handle that stops it.
struct ApiServer {
    port: u16,
    app: Arc<App>,
    shutdown: CancellationToken,
    _dir: tempfile::TempDir,
}

impl Drop for ApiServer {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// Start an API server with a freshly created gateway behind it.
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
    // The API is switched on with the test token.
    {
        let db = app.db.lock().await;
        pangolin_core::db::upsert_api_config(
            &db,
            &pangolin_core::db::ApiConfigRow {
                id: 1,
                instance_id: "test-instance".into(),
                token_hash: pangolin_core::db::sha256_hex(TOKEN),
                dashboard_port: 443,
                dashboard_ip: None,
                enabled: true,
                updated_at: chrono::Utc::now().to_rfc3339(),
            },
        )
        .expect("seed the api_config row");
    }

    let port = crate::harness::free_port();
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().expect("address");
    let state = Arc::new(api::state_with_key(app.clone(), test_key()));
    let shutdown = CancellationToken::new();
    let token = shutdown.clone();
    tokio::spawn(async move {
        let _ = api::http::serve(addr, state, token).await;
    });

    // Wait for the listener.
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    ApiServer {
        port,
        app,
        shutdown,
        _dir: dir,
    }
}

impl ApiServer {
    /// The base URL of the API.
    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Send a signed request.
    async fn send(&self, req: SignedRequest) -> (u16, String) {
        send_raw(&self.url(), req).await
    }

    /// Send a signed request and parse the JSON answer.
    async fn send_json(&self, req: SignedRequest) -> (u16, serde_json::Value) {
        let (status, body) = self.send(req).await;
        let json = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
        (status, json)
    }
}

/// Put a signed request on the wire.
async fn send_raw(base: &str, req: SignedRequest) -> (u16, String) {
    let client = reqwest::Client::new();
    let method = reqwest::Method::from_bytes(req.method.as_bytes()).expect("the method is valid");
    let url = format!("{base}{}", req.path);
    let mut builder = client.request(method, url);
    for (name, value) in req.headers.iter() {
        builder = builder.header(name, value);
    }
    let response = builder
        .body(req.body.clone())
        .send()
        .await
        .expect("the request goes through");
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    (status, body)
}

/// A client that signs with the test key.
fn client() -> ApiClient {
    ApiClient::new(TOKEN, test_seed())
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
    let (status, json) = server
        .send_json(client().post("/api/v1/sites", body.as_bytes()))
        .await;
    assert_eq!(status, 201, "{json}");
}

// ── The round trip ──────────────────────────────────────────────────────

#[tokio::test]
async fn a_signed_request_reads_the_sites() {
    let server = start_api().await;
    let (status, json) = server.send_json(client().get("/api/v1/sites")).await;
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

    let (status, json) = server
        .send_json(client().post("/api/v1/sites", body.as_bytes()))
        .await;
    assert_eq!(status, 201, "{json}");
    assert_eq!(json["name"], "example");
    assert_eq!(json["backend"], "http://127.0.0.1:8080");

    let (status, json) = server
        .send_json(client().get("/api/v1/sites/example"))
        .await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["name"], "example");

    let (status, json) = server
        .send_json(client().delete("/api/v1/sites/example"))
        .await;
    assert_eq!(status, 200, "{json}");

    let (status, json) = server
        .send_json(client().get("/api/v1/sites/example"))
        .await;
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
    let (status, _) = server
        .send_json(client().post("/api/v1/sites", body.as_bytes()))
        .await;
    assert_eq!(status, 201);
    let (status, json) = server
        .send_json(client().post("/api/v1/sites", body.as_bytes()))
        .await;
    assert_eq!(status, 409, "{json}");
    assert_eq!(error_code(&json), "site_exists");
}

#[tokio::test]
async fn a_site_without_a_backend_is_refused() {
    // The admin's own message, verbatim (issue #185, R11).
    let server = start_api().await;
    let body = serde_json::json!({ "name": "example" }).to_string();
    let (status, json) = server
        .send_json(client().post("/api/v1/sites", body.as_bytes()))
        .await;
    assert_eq!(status, 422, "{json}");
    assert_eq!(error_code(&json), "backend_invalid");
    assert_eq!(
        json["error"]["message"],
        "Backend is required — fill in the host:port (or file path) field"
    );
    assert_eq!(json["error"]["field"], "backend");
}

// ── The authentication ───────────────────────────────────────────────────

#[tokio::test]
async fn a_request_without_the_credentials_is_malformed() {
    let server = start_api().await;
    let client = reqwest::Client::new();
    let response = client
        .get(format!("{}/api/v1/sites", server.url()))
        .send()
        .await
        .expect("the request goes through");
    assert_eq!(response.status().as_u16(), 400);
    let json: serde_json::Value = response.json().await.expect("json");
    assert_eq!(error_code(&json), "malformed_request");
}

#[tokio::test]
async fn a_request_with_a_wrong_token_is_refused() {
    let server = start_api().await;
    let other = ApiClient::new("not-the-token", test_seed());
    let (status, json) = server.send_json(other.get("/api/v1/sites")).await;
    assert_eq!(status, 401, "{json}");
    assert_eq!(error_code(&json), "invalid_token");
}

#[tokio::test]
async fn a_request_signed_with_another_key_is_refused() {
    let server = start_api().await;
    let other = ApiClient::new(TOKEN, [42u8; 32]);
    let (status, json) = server.send_json(other.get("/api/v1/sites")).await;
    assert_eq!(status, 401, "{json}");
    assert_eq!(error_code(&json), "invalid_signature");
}

#[tokio::test]
async fn a_tampered_body_is_refused() {
    let server = start_api().await;
    let signed = client().post("/api/v1/sites", b"{\"name\":\"a\"}");
    let mut tampered = signed.clone();
    tampered.body = b"{\"name\":\"b\"}".to_vec();
    let (status, json) = server.send_json(tampered).await;
    assert_eq!(status, 401, "{json}");
    assert_eq!(error_code(&json), "invalid_signature");
}

#[tokio::test]
async fn a_tampered_path_is_refused() {
    let server = start_api().await;
    let signed = client().get("/api/v1/sites");
    let mut tampered = signed.clone();
    tampered.path = "/api/v1/domains".to_string();
    let (status, json) = server.send_json(tampered).await;
    assert_eq!(status, 401, "{json}");
    assert_eq!(error_code(&json), "invalid_signature");
}

#[tokio::test]
async fn a_tampered_timestamp_is_refused() {
    let server = start_api().await;
    let signed = client().get("/api/v1/sites");
    let mut tampered = signed.clone();
    let mut headers = signed.headers.clone();
    headers.insert(
        "x-pangolin-dashboard-ts",
        (api::protocol::auth::now_secs() - 3_600)
            .to_string()
            .parse()
            .expect("header"),
    );
    tampered.headers = headers;
    let (status, json) = server.send_json(tampered).await;
    assert_eq!(status, 401, "{json}");
    assert_eq!(error_code(&json), "timestamp_out_of_window");
}

#[tokio::test]
async fn the_same_signed_request_twice_is_a_replay() {
    let server = start_api().await;
    let signed = client().get("/api/v1/sites");
    let (status, _) = server.send_json(signed.clone()).await;
    assert_eq!(status, 200);
    let (status, json) = server.send_json(signed).await;
    assert_eq!(status, 401, "{json}");
    assert_eq!(error_code(&json), "replayed_nonce");
}

// ── The namespace ───────────────────────────────────────────────────────

#[tokio::test]
async fn the_admin_routes_do_not_exist_on_the_api_listener() {
    let server = start_api().await;
    for path in ["/", "/sites", "/settings", "/api/logs/stream"] {
        let (status, json) = server.send_json(client().get(path)).await;
        assert_eq!(status, 404, "{path} -> {json}");
    }
}

#[tokio::test]
async fn an_unknown_endpoint_is_a_not_found() {
    let server = start_api().await;
    let (status, json) = server.send_json(client().get("/api/v1/nope")).await;
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
        .send_json(client().post("/api/v1/dns_providers", body.as_bytes()))
        .await;
    assert_eq!(status, 201, "{json}");
    assert_eq!(json["has_api_token"], true);

    let (status, body) = server.send(client().get("/api/v1/dns_providers")).await;
    assert_eq!(status, 200);
    assert!(!body.contains("super-secret"), "{body}");
}

#[tokio::test]
async fn a_tun_never_echoes_its_token_but_returns_it_once() {
    let server = start_api().await;
    let body = serde_json::json!({ "name": "office" }).to_string();
    let (status, json) = server
        .send_json(client().post("/api/v1/tun", body.as_bytes()))
        .await;
    assert_eq!(status, 201, "{json}");
    // The token is in the create answer — and nowhere else.
    let token = json["token"].as_str().expect("the token is returned");
    assert!(!token.is_empty());

    let (status, body) = server.send(client().get("/api/v1/tun")).await;
    assert_eq!(status, 200);
    assert!(!body.contains(token), "{body}");
    assert!(body.contains("\"has_token\":true"), "{body}");
}

// ── The write semantics ─────────────────────────────────────────────────

#[tokio::test]
async fn the_system_config_is_merged() {
    let server = start_api().await;
    // Set a value first.
    let body = serde_json::json!({
        "provider": "cloudflare",
        "client_ip_header": "CF-Connecting-IP",
    })
    .to_string();
    let (status, json) = server
        .send_json(client().put("/api/v1/system_config", body.as_bytes()))
        .await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["provider"], "cloudflare");

    // A second write that mentions nothing keeps it.
    let body = serde_json::json!({ "pgl_headers_enabled": false }).to_string();
    let (status, json) = server
        .send_json(client().put("/api/v1/system_config", body.as_bytes()))
        .await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["provider"], "cloudflare");
    assert_eq!(json["client_ip_header"], "CF-Connecting-IP");
    assert_eq!(json["pgl_headers_enabled"], false);
}

#[tokio::test]
async fn the_bot_list_is_replaced() {
    let server = start_api().await;
    create_site(&server, "example").await;
    let body = serde_json::json!({ "bots": ["gptbot", "ccbot"] }).to_string();
    let (status, json) = server
        .send_json(client().put("/api/v1/site_rate_limit_bots/example", body.as_bytes()))
        .await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["bots"], serde_json::json!(["ccbot", "gptbot"]));

    // A second write with fewer entries drops the rest.
    let body = serde_json::json!({ "bots": ["gptbot"] }).to_string();
    let (status, json) = server
        .send_json(client().put("/api/v1/site_rate_limit_bots/example", body.as_bytes()))
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
        .send_json(client().put("/api/v1/site_origin_allow/example", body.as_bytes()))
        .await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["cidrs"], serde_json::json!(["1.2.3.0/24"]));

    let body = serde_json::json!({ "cidrs": [] }).to_string();
    let (status, json) = server
        .send_json(client().put("/api/v1/site_origin_allow/example", body.as_bytes()))
        .await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["cidrs"], serde_json::json!([]));
}

#[tokio::test]
async fn a_reputation_key_is_merged_and_can_be_cleared() {
    let server = start_api().await;
    let body = serde_json::json!({ "abuseipdb_api_key": "key-1" }).to_string();
    let (status, json) = server
        .send_json(client().put("/api/v1/badbots_config", body.as_bytes()))
        .await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["has_abuseipdb_api_key"], true);
    assert_eq!(json["has_crowdsec_api_key"], false);

    // A write that mentions nothing keeps the key.
    let body = serde_json::json!({ "enabled": false }).to_string();
    let (status, json) = server
        .send_json(client().put("/api/v1/badbots_config", body.as_bytes()))
        .await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["has_abuseipdb_api_key"], true);
    assert_eq!(json["enabled"], false);

    // `null` clears it.
    let body = serde_json::json!({ "abuseipdb_api_key": null }).to_string();
    let (status, json) = server
        .send_json(client().put("/api/v1/badbots_config", body.as_bytes()))
        .await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["has_abuseipdb_api_key"], false);
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
    let (status, json) = server
        .send_json(client().post("/api/v1/sites", body.as_bytes()))
        .await;
    assert_eq!(status, 400, "{json}");
    assert_eq!(error_code(&json), "malformed_request");
}

// ── The instance and the logs ───────────────────────────────────────────

#[tokio::test]
async fn the_instance_describes_the_node() {
    let server = start_api().await;
    let (status, json) = server.send_json(client().get("/api/v1/instance")).await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["instance_id"], "test-instance");
    assert_eq!(json["version"], pangolin_core::VERSION);
    assert!(json["uptime_seconds"].is_number(), "{json}");
    assert_eq!(
        json["dashboard_key_fingerprint"],
        api::protocol::signature::fingerprint(&test_key())
    );
    assert_eq!(json["dashboard"]["port"], 443);
    assert!(json["capabilities"].is_array(), "{json}");
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

    let (status, json) = server
        .send_json(client().get("/api/v1/logs/bot/dates"))
        .await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(
        json,
        serde_json::json!([date.format("%Y-%m-%d").to_string()])
    );

    let path = format!("/api/v1/logs/bot/raw?date={}", date.format("%Y-%m-%d"));
    let (status, body) = server.send(client().get(&path)).await;
    assert_eq!(status, 200);
    assert_eq!(body.as_bytes(), content);
}

#[tokio::test]
async fn a_log_date_that_is_not_a_date_is_refused() {
    let server = start_api().await;
    let (status, json) = server
        .send_json(client().get("/api/v1/logs/bot/raw?date=yesterday"))
        .await;
    assert_eq!(status, 422, "{json}");
    assert_eq!(error_code(&json), "date_invalid");
}

#[tokio::test]
async fn the_access_log_is_empty_when_nothing_happened() {
    let server = start_api().await;
    let (status, json) = server.send_json(client().get("/api/v1/logs/access")).await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json, serde_json::json!([]));
}

// ── The switch ──────────────────────────────────────────────────────────

// ── The guards the review found ─────────────────────────────────────────

#[tokio::test]
async fn a_rotated_token_is_the_one_the_tunnel_uses() {
    // The stored digest has to follow the new token: `upsert_tun`
    // prefers the digest it already has, so a rotation that does not
    // drop it leaves the tunnel on the old credential.
    let server = start_api().await;
    let body = serde_json::json!({ "name": "office" }).to_string();
    let (status, json) = server
        .send_json(client().post("/api/v1/tun", body.as_bytes()))
        .await;
    assert_eq!(status, 201, "{json}");
    let old_token = json["token"].as_str().expect("the token is returned").to_string();

    let body = serde_json::json!({ "token": "the-new-token" }).to_string();
    let (status, json) = server
        .send_json(client().put("/api/v1/tun/office", body.as_bytes()))
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
async fn an_oversized_body_is_refused() {
    // The body is read before the authentication gate runs, so an
    // unauthenticated client must not be able to make the node buffer
    // an arbitrary amount of memory.
    let server = start_api().await;
    let body = vec![b'x'; api::http::MAX_BODY_BYTES + 1];
    let (status, _) = server.send(client().post("/api/v1/sites", &body)).await;
    assert_eq!(status, 413);
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
    let (status, _) = server
        .send_json(client().post("/api/v1/sites", body.as_bytes()))
        .await;
    assert_eq!(status, 201);

    let body = serde_json::json!({
        "scope": "example",
        "kind": "blacklist",
        "cidr": "10.0.0.0/8",
    })
    .to_string();
    let (status, json) = server
        .send_json(client().post("/api/v1/ip_rules", body.as_bytes()))
        .await;
    assert_eq!(status, 201, "{json}");
    assert_eq!(json["id"], "example|blacklist|10.0.0.0/8");

    let (status, json) = server
        .send_json(client().delete("/api/v1/ip_rules/example|blacklist|10.0.0.0/8"))
        .await;
    assert_eq!(status, 200, "{json}");

    let (status, json) = server.send_json(client().get("/api/v1/ip_rules")).await;
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
        .send_json(client().put("/api/v1/site_rate_limits/nope", body.as_bytes()))
        .await;
    assert_eq!(status, 404, "{json}");
    assert_eq!(error_code(&json), "site_not_found");
}

#[tokio::test]
async fn a_disabled_api_refuses_every_call() {
    let server = start_api().await;
    {
        let db = server.app.db.lock().await;
        let mut row = pangolin_core::db::get_api_config(&db)
            .expect("the row")
            .expect("the row");
        row.enabled = false;
        pangolin_core::db::upsert_api_config(&db, &row).expect("write");
    }
    let (status, json) = server.send_json(client().get("/api/v1/sites")).await;
    assert_eq!(status, 403, "{json}");
    assert_eq!(error_code(&json), "api_disabled");
}
