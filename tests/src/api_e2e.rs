//! The dashboard-facing management API — the end-to-end suite
//! (issue #185).
//!
//! Unlike [`crate::api`], this suite drives the **real**
//! `pangolin-ngx` binary: the listener is the one the shipped
//! gateway starts, and the verifying key is the one **compiled into
//! the binary**.
//!
//! The signing seed lives in `tests/fixtures/` — the development key
//! pair the shipped binary trusts. The production key replaces both
//! files before the first release that ships the feature; the test
//! below fails loudly when the two drift apart.

use std::path::Path;

use api::client::ApiClient;
use pangolin_core::db::ApiConfigRow;

/// The token the end-to-end suite authenticates with.
const TOKEN: &str = "the-e2e-token";

/// The development seed — the private key of the key pair the binary
/// trusts.
fn dev_seed() -> [u8; 32] {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/dashboard_dev_seed.hex");
    let text = std::fs::read_to_string(&path).expect("the development seed is in the repository");
    let bytes = hex::decode(text.trim()).expect("the seed is hex");
    assert_eq!(bytes.len(), 32, "the seed is 32 bytes");
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&bytes);
    seed
}

/// The client that signs with the development key.
fn client() -> ApiClient {
    ApiClient::new(TOKEN, dev_seed())
}

/// The development seed must match the key the binary trusts.
///
/// Otherwise the suite would sign with a key the node does not know,
/// and every positive test would fail for the wrong reason.
#[test]
fn the_development_seed_matches_the_embedded_key() {
    let derived =
        api::protocol::signature::public_key_from_seed(&dev_seed()).expect("the seed yields a key");
    assert_eq!(
        derived,
        api::protocol::signature::embedded_dashboard_key(),
        "tests/fixtures/dashboard_dev_seed.hex no longer matches \
         crates/api/src/dashboard.pub — replace both together"
    );
}

/// Seed the API's configuration into the database the gateway is
/// about to open.
fn seed_api_config(db_path: &Path) {
    let conn = pangolin_core::db::open(db_path).expect("open pangolin.db");
    conn.execute(
        "INSERT INTO api_config (id, instance_id, token_hash, dashboard_port, dashboard_ip, enabled, updated_at)
         VALUES (1, 'e2e-instance', ?1, 443, NULL, 1, ?2)",
        rusqlite::params![
            pangolin_core::db::sha256_hex(TOKEN),
            chrono::Utc::now().to_rfc3339()
        ],
    )
    .expect("seed the api_config row");
}

/// Put a signed request on the wire.
async fn send(base: &str, req: api::client::SignedRequest) -> (u16, String) {
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

#[tokio::test]
async fn real_e2e_api_creates_reads_and_deletes_a_site() {
    let ngx = crate::harness::NgxProcess::start(|db_path| {
        crate::harness::init_pangolin_db(db_path);
        seed_api_config(db_path);
    })
    .await;

    let base = format!("http://127.0.0.1:{}", ngx.api_port);
    let body = serde_json::json!({
        "name": "e2e-site",
        "backend": "http://127.0.0.1:9999",
    })
    .to_string();

    let (status, answer) = send(&base, client().post("/api/v1/sites", body.as_bytes())).await;
    assert_eq!(status, 201, "{answer}");
    assert!(answer.contains("e2e-site"), "{answer}");

    let (status, answer) = send(&base, client().get("/api/v1/sites/e2e-site")).await;
    assert_eq!(status, 200, "{answer}");
    assert!(answer.contains("e2e-site"), "{answer}");

    let (status, answer) = send(&base, client().delete("/api/v1/sites/e2e-site")).await;
    assert_eq!(status, 200, "{answer}");

    let (status, answer) = send(&base, client().get("/api/v1/sites/e2e-site")).await;
    assert_eq!(status, 404, "{answer}");
}

#[tokio::test]
async fn real_e2e_api_refuses_an_unsigned_request() {
    let ngx = crate::harness::NgxProcess::start(|db_path| {
        crate::harness::init_pangolin_db(db_path);
        seed_api_config(db_path);
    })
    .await;

    let base = format!("http://127.0.0.1:{}", ngx.api_port);
    let client = reqwest::Client::new();
    let response = client
        .get(format!("{base}/api/v1/sites"))
        .send()
        .await
        .expect("the request goes through");
    assert_eq!(response.status().as_u16(), 400);
    let json: serde_json::Value = response.json().await.expect("json");
    assert_eq!(json["error"]["code"], "malformed_request");
}

#[tokio::test]
async fn real_e2e_api_refuses_a_request_signed_with_another_key() {
    let ngx = crate::harness::NgxProcess::start(|db_path| {
        crate::harness::init_pangolin_db(db_path);
        seed_api_config(db_path);
    })
    .await;

    let base = format!("http://127.0.0.1:{}", ngx.api_port);
    let other = ApiClient::new(TOKEN, [99u8; 32]);
    let (status, answer) = send(&base, other.get("/api/v1/sites")).await;
    assert_eq!(status, 401, "{answer}");
    assert!(answer.contains("invalid_signature"), "{answer}");
}

#[tokio::test]
async fn real_e2e_api_does_not_serve_the_admin_routes() {
    let ngx = crate::harness::NgxProcess::start(|db_path| {
        crate::harness::init_pangolin_db(db_path);
        seed_api_config(db_path);
    })
    .await;

    let base = format!("http://127.0.0.1:{}", ngx.api_port);
    let (status, answer) = send(&base, client().get("/sites")).await;
    assert_eq!(status, 404, "{answer}");
}

#[tokio::test]
async fn real_e2e_api_refuses_every_call_when_it_is_switched_off() {
    let ngx = crate::harness::NgxProcess::start(|db_path| {
        crate::harness::init_pangolin_db(db_path);
        seed_api_config(db_path);
        let conn = pangolin_core::db::open(db_path).expect("open pangolin.db");
        conn.execute("UPDATE api_config SET enabled = 0 WHERE id = 1", [])
            .expect("switch the API off");
    })
    .await;

    let base = format!("http://127.0.0.1:{}", ngx.api_port);
    let (status, answer) = send(&base, client().get("/api/v1/sites")).await;
    assert_eq!(status, 403, "{answer}");
    assert!(answer.contains("api_disabled"), "{answer}");
}

/// The seeded row has to carry what the API reads.
#[test]
fn the_seeded_row_has_the_shape_the_api_reads() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pangolin.db");
    crate::harness::init_pangolin_db(&path);
    seed_api_config(&path);

    let conn = pangolin_core::db::open(&path).expect("open");
    let row: ApiConfigRow = pangolin_core::db::get_api_config(&conn)
        .expect("read")
        .expect("the row");
    assert_eq!(row.instance_id, "e2e-instance");
    assert!(row.enabled);
    assert_eq!(row.token_hash, pangolin_core::db::sha256_hex(TOKEN));
}
