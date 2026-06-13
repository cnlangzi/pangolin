#![allow(unused)]
//! ACME integration test using Pebble (local ACME test server).
//!
//! Start Pebble:
//!   podman run -d --rm --name pebble-acme \
//!     -p 14000:14000 -p 15000:15000 \
//!     -e PEBBLE_VA_NOSLEEP=1 -e PEBBLE_VA_ALWAYS_VALID=1 \
//!     ghcr.io/letsencrypt/pebble:latest
//!   echo "127.0.0.1 localhost.test" | sudo tee -a /etc/hosts
//!
//! Run with: cargo test --features integration -p ngx

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use tempfile::TempDir;
use tokio::time::{timeout, Duration};

// Force ring as the default crypto provider for tests.
// rustls 0.23 auto-detects, but when both ring and aws-lc-rs are linked
// (ring via rcgen; aws-lc-rs via rustls-native-certs from hyper-rustls),
// we must select explicitly. Registration is per-process, first wins.
// Delegated to `pangolin_core::install_crypto_provider` so the binary
// + every test harness routes through one helper.
#[ctor::ctor]
fn init_crypto() {
    pangolin_core::install_crypto_provider();
}

/// Pebble root CA (self-signed, from letsencrypt/pebble test/config/pebble-root.cert.pem).
/// DO NOT use in production.
static PEBBLE_ROOT_CA: &[u8] = include_bytes!("./pebble-root.pem");

const ACME_DIR: &str = "https://localhost:14000/dir";
const TEST_EMAIL: &str = "test@example.com";

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

/// Fix Pebble's authZ response by removing challenges without tokens.
fn patch_authz_response(body: Vec<u8>) -> Vec<u8> {
    if let Ok(text) = std::str::from_utf8(&body) {
        if text.contains("\"challenges\"") && text.contains("\"dns-persist-01\"") {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(text) {
                if let Some(challenges) = json.get("challenges").and_then(|c| c.as_array()) {
                    let filtered: Vec<_> = challenges
                        .iter()
                        .filter(|c| {
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

/// Wrap reqwest to implement `instant_acme::HttpClient`.
///
/// Only compiled when the `integration` feature is on (Pebble
/// test runs are gated by this feature in the workspace
/// `make test-e2e` target). The 0.8.x `HttpClient` trait
/// uses `Request<BodyWrapper<Bytes>>` as the request body
/// type (not `Full<Bytes>` as in 0.7.x) — we extract the
/// raw bytes from the incoming BodyWrapper via
/// `http_body_util::BodyExt::collect` and feed them to
/// reqwest, then re-wrap the response as `BodyWrapper<Bytes>`
/// via the `From<Vec<u8>>` impl for the response.
#[cfg(feature = "integration")]
struct AcmeHttpClient(reqwest::Client);

#[cfg(feature = "integration")]
impl instant_acme::HttpClient for AcmeHttpClient {
    fn request(
        &self,
        req: http::Request<instant_acme::BodyWrapper<Bytes>>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<instant_acme::BytesResponse, instant_acme::Error>,
                > + Send,
        >,
    > {
        let client = self.0.clone();
        Box::pin(async move {
            let method = req.method().clone();
            let uri = req.uri().to_string();
            let headers = req.headers().clone();
            let body_bytes: Vec<u8> = req
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
                .map_err(|e| instant_acme::Error::Other(e.into()))?
                .to_vec();

            let body_bytes: bytes::Bytes = if uri.contains("authZ") {
                patch_authz_response(body_bytes).into()
            } else {
                body_bytes.into()
            };

            let mut response = hyper::Response::new(Full::new(body_bytes));
            *response.status_mut() = status;
            *response.headers_mut() = headers;

            Ok(instant_acme::BytesResponse::from(response))
        })
    }
}

/// Build an AcmeClient configured for ECDSA HTTP-01 (matches previous test setup).
#[cfg(feature = "integration")]
async fn build_ecdsa_http01_client(
    cert_dir: std::path::PathBuf,
) -> anyhow::Result<Arc<ngx::acme::AcmeClient>> {
    let http_client = build_acme_client(PEBBLE_ROOT_CA).expect("build HTTP client");
    Ok(Arc::new(
        ngx::acme::AcmeClient::with_http_client(
            Box::new(AcmeHttpClient(http_client)),
            cert_dir,
            TEST_EMAIL.to_string(),
            ACME_DIR,
            7,
            24,
            ngx::acme::KeyType::Ecdsa,
            None, // no DNS provider → HTTP-01
        )
        .await
        .expect("ACME client init"),
    ))
}

/// Build an AcmeClient configured for RSA HTTP-01.
#[cfg(feature = "integration")]
async fn build_rsa_http01_client(
    cert_dir: std::path::PathBuf,
) -> anyhow::Result<Arc<ngx::acme::AcmeClient>> {
    let http_client = build_acme_client(PEBBLE_ROOT_CA).expect("build HTTP client");
    Ok(Arc::new(
        ngx::acme::AcmeClient::with_http_client(
            Box::new(AcmeHttpClient(http_client)),
            cert_dir,
            TEST_EMAIL.to_string(),
            ACME_DIR,
            7,
            24,
            ngx::acme::KeyType::Rsa,
            None,
        )
        .await
        .expect("ACME client init"),
    ))
}

/// Read the perms of a file and return (mode, owner_uid) on Unix.
#[cfg(unix)]
fn file_mode(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).expect("stat").permissions().mode() & 0o777
}

// =============================================================================
// E2E: ECDSA single-domain cert
// =============================================================================
#[cfg(feature = "integration")]
#[tokio::test]
async fn acme_issue_ecdsa_single_domain() {
    let cert_dir = TempDir::new().expect("temp cert dir");
    let cert_dir_path = cert_dir.path().to_path_buf();

    let test_future = async {
        let client = build_ecdsa_http01_client(cert_dir_path.clone())
            .await
            .expect("client init");

        let domains = vec!["localhost.test".to_string()];
        let results = client.issue_cert(&domains).await.expect("issue failed");
        assert_eq!(results.len(), 1, "single SAN → 1 blob pair");

        let (cert_path, key_path) = &results[0];
        assert!(cert_path.exists(), "cert blob not created");
        assert!(key_path.exists(), "key blob not created");
        // In blob mode, cert_path == key_path (same file).
        assert_eq!(cert_path, key_path, "blob is single file");

        // File lives at cert_dir/{san}, no `+rsa` suffix (ECDSA default).
        let expected = cert_dir_path.join("localhost.test");
        assert_eq!(cert_path, &expected, "blob at autocert DirCache location");

        // Mode is 0600 (proxy mux compatibility).
        assert_eq!(file_mode(cert_path), 0o600, "blob must be 0600");

        // Blob byte-format check: SEC1 EC PRIVATE KEY at the top, then one or more
        // CERTIFICATE blocks. This is the byte-level contract with proxy mux.
        let blob = std::fs::read_to_string(cert_path).expect("read blob");
        assert!(
            blob.contains("-----BEGIN EC PRIVATE KEY-----"),
            "blob must begin with SEC1 EC PRIVATE KEY"
        );
        assert!(
            blob.contains("-----END EC PRIVATE KEY-----"),
            "blob must contain SEC1 EC PRIVATE KEY end marker"
        );
        let cert_blocks = blob.matches("-----BEGIN CERTIFICATE-----").count();
        assert!(
            cert_blocks >= 1,
            "blob must contain at least one CERTIFICATE block (got {})",
            cert_blocks
        );

        // acme_account.json file written (renamed from acme_account+key
        // in issue #45 follow-up — the new extension makes the format
        // obvious to operators / tooling).
        let account_file = cert_dir_path.join("acme_account.json");
        assert!(account_file.exists(), "acme_account.json not written");
        assert_eq!(file_mode(&account_file), 0o600, "account file 0600");
        let account = std::fs::read_to_string(&account_file).expect("read account");
        // instant-acme AccountCredentials JSON fields: id, key_pkcs8 (base64 string), directory
        assert!(account.contains("\"id\""), "account JSON missing id");
        assert!(
            account.contains("\"key_pkcs8\""),
            "account JSON missing key_pkcs8"
        );
        assert!(
            account.contains("\"directory\""),
            "account JSON missing directory: {}",
            &account[..account.len().min(160)]
        );

        // Issue complete — clear cleanup before temp dir drop
        // (acme.rs removes HTTP-01 challenge files; we just check no panic).
    };

    timeout(Duration::from_secs(60), test_future)
        .await
        .expect("acme_issue_ecdsa_single_domain timed out after 60s");
}

// =============================================================================
// E2E: RSA variant → blob filename is {domain}+rsa
// =============================================================================
#[cfg(feature = "integration")]
#[tokio::test]
async fn acme_issue_rsa_variant_filename() {
    let cert_dir = TempDir::new().expect("temp cert dir");
    let cert_dir_path = cert_dir.path().to_path_buf();

    let test_future = async {
        let client = build_rsa_http01_client(cert_dir_path.clone())
            .await
            .expect("client init");

        let domains = vec!["localhost.test".to_string()];
        let results = client.issue_cert(&domains).await.expect("issue failed");

        let (cert_path, _) = &results[0];
        let expected = cert_dir_path.join("localhost.test+rsa");
        assert_eq!(
            cert_path, &expected,
            "RSA blob must be at {{domain}}+rsa, not {{domain}}"
        );
        assert!(cert_path.exists(), "RSA blob not created");

        // ECDSA default blob must NOT exist when key_type=rsa
        let ecdsa_default = cert_dir_path.join("localhost.test");
        assert!(
            !ecdsa_default.exists(),
            "RSA mode must not also write ECDSA-default blob"
        );

        // Blob contains PKCS#1 RSA PRIVATE KEY (not SEC1 EC)
        let blob = std::fs::read_to_string(cert_path).expect("read blob");
        assert!(
            blob.contains("-----BEGIN RSA PRIVATE KEY-----"),
            "RSA blob must use PKCS#1 RSA PRIVATE KEY"
        );
        assert!(
            !blob.contains("-----BEGIN EC PRIVATE KEY-----"),
            "RSA blob must not contain EC key"
        );

        // Permissions 0600
        assert_eq!(file_mode(cert_path), 0o600, "RSA blob must be 0600");
    };

    timeout(Duration::from_secs(60), test_future)
        .await
        .expect("acme_issue_rsa_variant_filename timed out after 60s");
}

// =============================================================================
// E2E: multi-SAN cert → one blob per SAN, identical content
// =============================================================================
// Pebble signs CN-only (not SAN-matching), so we use 2 SANs that resolve to 127.0.0.1.
// The HTTP-01 challenge is per-domain, but Pebble (with PEBBLE_VA_ALWAYS_VALID=1)
// doesn't actually validate.
#[cfg(feature = "integration")]
#[tokio::test]
async fn acme_issue_multi_san_blob_copy() {
    let cert_dir = TempDir::new().expect("temp cert dir");
    let cert_dir_path = cert_dir.path().to_path_buf();

    let test_future = async {
        let client = build_ecdsa_http01_client(cert_dir_path.clone())
            .await
            .expect("client init");

        // Use 2 SANs both pointing to localhost.test in /etc/hosts.
        // Pebble signs whatever the order asks for.
        let domains = vec![
            "localhost.test".to_string(),
            "alt.localhost.test".to_string(),
        ];
        // /etc/hosts maps localhost.test → 127.0.0.1; add alt.localhost.test too.
        // (already covered by the *.test catch-all if Pebble accepts it; we
        // rely on PEBBLE_VA_ALWAYS_VALID=1 to skip DNS resolution anyway)

        let results = client.issue_cert(&domains).await.expect("issue failed");
        assert_eq!(results.len(), 2, "2 SANs → 2 blob pairs");

        // Two separate files, both with identical blob content.
        let blob1 = std::fs::read(&results[0].0).expect("read blob 1");
        let blob2 = std::fs::read(&results[1].0).expect("read blob 2");
        assert_eq!(blob1, blob2, "multi-SAN blob copies must be byte-identical");

        // Filenames are SAN names, not domain.com/fullchain.pem
        let expected1 = cert_dir_path.join("localhost.test");
        let expected2 = cert_dir_path.join("alt.localhost.test");
        assert_eq!(results[0].0, expected1);
        assert_eq!(results[1].0, expected2);
        assert!(expected1.exists());
        assert!(expected2.exists());
    };

    timeout(Duration::from_secs(60), test_future)
        .await
        .expect("acme_issue_multi_san_blob_copy timed out after 60s");
}

// =============================================================================
// E2E: ACME account persistence — acme_account.json survives restart
// =============================================================================
#[cfg(feature = "integration")]
#[tokio::test]
async fn acme_account_persistence_across_restart() {
    let cert_dir = TempDir::new().expect("temp cert dir");
    let cert_dir_path = cert_dir.path().to_path_buf();

    // First boot: create account
    {
        let client = build_ecdsa_http01_client(cert_dir_path.clone())
            .await
            .expect("first boot client");
        // Don't even need to issue a cert — just init the client.
        // The AcmeClient::new writes acme_account.json on first init.
        drop(client);
    }

    let account_file = cert_dir_path.join("acme_account.json");
    assert!(account_file.exists(), "account file written on first init");
    let first_contents = std::fs::read_to_string(&account_file).expect("read account");
    assert!(first_contents.contains("\"id\""));
    assert!(first_contents.contains("\"key_pkcs8\""));

    // Second boot: same cert_dir, account should be reloaded (not re-registered)
    {
        let http_client = build_acme_client(PEBBLE_ROOT_CA).expect("build HTTP client");
        let client = ngx::acme::AcmeClient::with_http_client(
            Box::new(AcmeHttpClient(http_client)),
            cert_dir_path.clone(),
            TEST_EMAIL.to_string(),
            ACME_DIR,
            7,
            24,
            ngx::acme::KeyType::Ecdsa,
            None,
        )
        .await
        .expect("second boot client");

        // Issue a cert using the loaded account
        let domains = vec!["localhost.test".to_string()];
        timeout(Duration::from_secs(60), client.issue_cert(&domains))
            .await
            .expect("issue timed out")
            .expect("issue failed");
    }

    // Account file unchanged (same id+key)
    let second_contents = std::fs::read_to_string(&account_file).expect("read account 2");
    assert_eq!(
        first_contents, second_contents,
        "account file must not be regenerated on second init"
    );
}

// =============================================================================
// E2E: cert_manager.resolve_cert → blob path, ECDSA first, then +rsa, then default
// =============================================================================
// These don't need Pebble; they exercise the local-fs path-resolution logic.
#[tokio::test]
async fn cert_manager_resolve_blob_ecdsa() {
    let cert_dir = TempDir::new().expect("temp cert dir");
    let cert_dir_path = cert_dir.path().to_path_buf();

    let cm = ngx::CertManager::new(
        cert_dir_path.clone(),
        TEST_EMAIL.to_string(),
        ACME_DIR.to_string(),
        7,
        24,
        3,
        "ecdsa".to_string(),
    );

    // Write an ECDSA blob (default filename, no suffix)
    let blob = "-----BEGIN EC PRIVATE KEY-----\nfake\n-----END EC PRIVATE KEY-----\n-----BEGIN CERTIFICATE-----\nfake\n-----END CERTIFICATE-----\n";
    let blob_path = cert_dir_path.join("test.example.com");
    std::fs::write(&blob_path, blob).expect("write blob");

    let (cert, key) = cm.resolve_cert("test.example.com").expect("resolve");
    assert_eq!(cert, blob_path.to_string_lossy());
    assert_eq!(key, blob_path.to_string_lossy(), "blob: cert==key");
}

#[tokio::test]
async fn cert_manager_resolve_blob_falls_back_to_rsa() {
    let cert_dir = TempDir::new().expect("temp cert dir");
    let cert_dir_path = cert_dir.path().to_path_buf();

    let cm = ngx::CertManager::new(
        cert_dir_path.clone(),
        TEST_EMAIL.to_string(),
        ACME_DIR.to_string(),
        7,
        24,
        3,
        "ecdsa".to_string(),
    );

    // Only RSA blob exists → resolver falls back
    let rsa_blob = cert_dir_path.join("test.example.com+rsa");
    std::fs::write(&rsa_blob, "fake").expect("write rsa blob");

    let (cert, key) = cm.resolve_cert("test.example.com").expect("resolve");
    assert_eq!(cert, rsa_blob.to_string_lossy());
    assert_eq!(key, rsa_blob.to_string_lossy());
}

#[tokio::test]
async fn cert_manager_resolve_blob_prefers_ecdsa_over_rsa() {
    let cert_dir = TempDir::new().expect("temp cert dir");
    let cert_dir_path = cert_dir.path().to_path_buf();

    let cm = ngx::CertManager::new(
        cert_dir_path.clone(),
        TEST_EMAIL.to_string(),
        ACME_DIR.to_string(),
        7,
        24,
        3,
        "ecdsa".to_string(),
    );

    // Both blobs exist — ECDSA default must win
    std::fs::write(cert_dir_path.join("test.example.com"), "ecdsa-blob").unwrap();
    std::fs::write(cert_dir_path.join("test.example.com+rsa"), "rsa-blob").unwrap();

    let (cert, _) = cm.resolve_cert("test.example.com").expect("resolve");
    assert!(
        cert.ends_with("test.example.com"),
        "ECDSA must be preferred, got {}",
        cert
    );
    assert!(!cert.contains("+rsa"));
}

#[tokio::test]
async fn cert_manager_no_default_fallback_in_v2() {
    // v2: there is no `default` blob fallback. Each host must have its own
    // cert on disk; otherwise the SNI handshake for that host fails.
    let cert_dir = TempDir::new().expect("temp cert dir");
    let cert_dir_path = cert_dir.path().to_path_buf();

    let cm = ngx::CertManager::new(
        cert_dir_path.clone(),
        TEST_EMAIL.to_string(),
        ACME_DIR.to_string(),
        7,
        24,
        3,
        "ecdsa".to_string(),
    );

    // No host-specific blob, but `default` exists — should still error.
    std::fs::write(cert_dir_path.join("default"), "default-blob").unwrap();

    let err = cm
        .resolve_cert("unknown.example.com")
        .expect_err("missing host cert must error in v2");
    assert!(
        err.to_string().contains("no certificate found"),
        "unexpected error: {err}"
    );
}
