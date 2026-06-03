//! ACME integration test using Pebble (local ACME test server).
//!
//! Start Pebble:
//!   podman run -d --name pebble -p 5001:5001 -p 14000:14000 -p 15000:15000 ghcr.io/letsencrypt/pebble:latest

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use tempfile::TempDir;

use tokio::time::{timeout, Duration};

/// Pebble root CA (self-signed, from letsencrypt/pebble test/config/pebble-root.cert.pem).
/// DO NOT use in production.
static PEBBLE_ROOT_CA: &[u8] = include_bytes!("../../../test-support/pebble-root.pem");

/// Build a reqwest client that trusts our custom CA for ACME operations.
fn build_acme_client(ca_pem: &[u8]) -> anyhow::Result<reqwest::Client> {
    let cert = reqwest::Certificate::from_pem(ca_pem)?;

    let client = reqwest::Client::builder()
        .add_root_certificate(cert)
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| anyhow::anyhow!("reqwest build error: {}", e))?;
    Ok(client)
}

/// Wrap reqwest to implement `instant_acme::HttpClient`.
/// Patches authZ responses to remove challenges without tokens (Pebble extensions).
struct AcmeHttpClient(reqwest::Client);

/// Fix Pebble's authZ response by removing challenges without tokens.
fn patch_authz_response(body: Vec<u8>) -> Vec<u8> {
    // Only process JSON responses
    if let Ok(text) = std::str::from_utf8(&body) {
        if text.contains("\"challenges\"") && text.contains("\"dns-persist-01\"") {
            // Use regex-like approach to remove challenges without "token" field
            // This is a simplified fix for Pebble's dns-persist-01 challenge
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(text) {
                if let Some(challenges) = json.get("challenges").and_then(|c| c.as_array()) {
                    let filtered: Vec<_> = challenges
                        .iter()
                        .filter(|c| {
                            // Keep challenges that have a non-empty token field
                            c.get("token")
                                .and_then(|t| t.as_str())
                                .map(|t| !t.is_empty())
                                .unwrap_or(false)
                        })
                        .cloned()
                        .collect();

                    if filtered.len() != challenges.len() {
                        let mut patched = json.clone();
                        patched["challenges"] = serde_json::Value::Array(filtered);
                        if let Ok(patched_str) = serde_json::to_string(&patched) {
                            return patched_str.into_bytes();
                        }
                    }
                }
            }
        }
    }
    body
}

impl instant_acme::HttpClient for AcmeHttpClient {
    fn request(
        &self,
        req: hyper::Request<Full<Bytes>>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<instant_acme::BytesResponse, instant_acme::Error>,
                > + Send,
        >,
    > {
        let client = self.0.clone();
        let uri = req.uri().to_string();
        let method = req.method().clone();
        let headers = req.headers().clone();

        Box::pin(async move {
            // Collect the request body
            let body_bytes = req
                .into_body()
                .collect()
                .await
                .map_err(|e| instant_acme::Error::Other(e.into()))?
                .to_bytes()
                .to_vec();

            let mut builder = client.request(
                reqwest::Method::from_bytes(method.as_str().as_bytes())
                    .unwrap_or(reqwest::Method::GET),
                &uri,
            );
            for (k, v) in headers.iter() {
                builder = builder.header(k.as_str(), v.to_str().unwrap_or(""));
            }
            // Pebble requires User-Agent
            builder = builder.header("User-Agent", "pangolin-acme-test/0.1");

            let resp = builder
                .body(body_bytes)
                .send()
                .await
                .map_err(|e| instant_acme::Error::Other(e.into()))?;

            let status =
                http::StatusCode::from_u16(resp.status().as_u16()).unwrap_or(http::StatusCode::OK);
            let headers = resp.headers().clone();
            let body_bytes = resp
                .bytes()
                .await
                .map_err(|e| instant_acme::Error::Other(e.into()))?;

            // Patch Pebble authZ responses to remove challenges without tokens
            let body_bytes: bytes::Bytes = if uri.contains("authZ") {
                patch_authz_response(body_bytes.to_vec()).into()
            } else {
                body_bytes.to_vec().into()
            };

            let mut response = hyper::Response::new(Full::new(body_bytes));
            *response.status_mut() = status;
            *response.headers_mut() = headers;

            Ok(instant_acme::BytesResponse::from(response))
        })
    }
}

// Note: These are integration tests that require:
// 1. Pebble ACME test server running on localhost:14000
// 2. /etc/hosts entry: "127.0.0.1 localhost.test"
// 3. HTTP server on port 5002 serving ACME challenge files
// Run manually with: cargo test -p ngx --test acme -- --ignored
#[ignore]
#[tokio::test]
async fn acme_issue_certificate() {
    let cert_dir = TempDir::new().expect("temp cert dir");
    let cert_dir_path = cert_dir.path().to_path_buf();

    let acme_dir = "https://localhost:14000/dir";

    let http_client = build_acme_client(PEBBLE_ROOT_CA).expect("build HTTP client");

    let client = Arc::new(
        ngx::acme::AcmeClient::with_http_client(
            Box::new(AcmeHttpClient(http_client)),
            cert_dir_path.clone(),
            "test@example.com".to_string(),
            acme_dir,
            7,
            24,
        )
        .await
        .expect("ACME client init"),
    );

    // Issue a certificate for localhost.test
    let domains = vec!["localhost.test".to_string()];
    let result = timeout(Duration::from_secs(60), client.issue_cert(&domains))
        .await
        .expect("ACME issue timed out")
        .expect("ACME issue failed");

    let (cert_path, key_path) = result;
    assert!(cert_path.exists(), "cert file not created");
    assert!(key_path.exists(), "key file not created");

    // Verify cert contents
    let cert_pem = std::fs::read_to_string(&cert_path).expect("read cert");
    assert!(cert_pem.contains("-----BEGIN CERTIFICATE-----"));
    assert!(cert_pem.contains("localhost.test"));

    let key_pem = std::fs::read_to_string(&key_path).expect("read key");
    assert!(key_pem.contains("-----BEGIN PRIVATE KEY-----"));

    println!(
        "ACME cert issued successfully: {} / {}",
        cert_path.display(),
        key_path.display()
    );
}

// Note: These tests require the same environment as acme_issue_certificate
// plus a working rustls ring crypto provider.
#[ignore]
#[tokio::test]
async fn cert_manager_resolve_existing() {
    let cert_dir = TempDir::new().expect("temp cert dir");
    let cert_dir_path = cert_dir.path().to_path_buf();

    let cm = ngx::acme::CertManager::new(
        true,
        cert_dir_path.clone(),
        "test@example.com".to_string(),
        "https://localhost:14000/dir".to_string(),
        7,
        24,
        3,
    );

    cm.init().await.expect("CM init");

    // Write a dummy cert/key to test resolve
    let host_dir = cert_dir_path.join("test.example.com");
    std::fs::create_dir_all(&host_dir).ok();
    std::fs::write(host_dir.join("fullchain.pem"), "dummy").ok();
    std::fs::write(host_dir.join("privkey.pem"), "dummy").ok();

    let (cert_r, key_r) = cm.resolve_cert("test.example.com").expect("resolve");
    assert!(cert_r.ends_with("fullchain.pem"));
    assert!(key_r.ends_with("privkey.pem"));
}
