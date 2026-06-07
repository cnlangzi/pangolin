//! Real-binary e2e tests.
//!
//! Unlike the rest of `tests/src/*.rs` (which test the `pangolin-core`
//! library with in-process mock backends), these tests drive the
//! **actual `pangolin-ngx` and `pangolin-tun` binaries** the way
//! production would: real subprocesses, real ports, real TLS, real
//! WebSocket upgrade, real CLI parsing.
//!
//! Run with: `cargo test --features integration -p pangolin-integration-tests real_e2e`
//!
//! Prerequisite: `make build` (or `cargo build --release -p ngx -p tun`)
//! so the binaries exist at `target/release/{ngx,tun}`.

use std::sync::Arc;

use chrono::{Duration as ChronoDuration, Utc};
use pangolin_core::db;
use rusqlite::Connection;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

use crate::harness::{init_pangolin_db, NgxProcess, TunProcess};

/// Issue a raw HTTP/1.1 GET to `addr` with the given `Host` header,
/// returning the response body. This bypasses reqwest entirely so
/// we have full control over the `Host` header (reqwest 0.12 makes
/// it nearly impossible to override the auto-derived Host value
/// when the URL is a numeric IP — see reqwest#686). The proxy
/// routes by Host, so getting this right is critical.
async fn raw_get(addr: &str, host: &str, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.expect("connect to proxy");
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nUser-Agent: pangolin-e2e\r\nAccept: */*\r\n\r\n"
    );
    stream
        .write_all(req.as_bytes())
        .await
        .expect("write request");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read response");
    let text = String::from_utf8_lossy(&buf).into_owned();
    // Parse the status line: "HTTP/1.1 200 OK\r\n"
    let status = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    // Drop the headers — split on the first "\r\n\r\n"
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

// ---------------------------------------------------------------------------
// Minimal in-process mock HTTP backend for tunnel tests.
// ---------------------------------------------------------------------------

struct MockBackend {
    addr: String,
    requests: Arc<Mutex<Vec<String>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl MockBackend {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let reqs_for_task = requests.clone();
        let handle = tokio::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let reqs = reqs_for_task.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let n = match stream.read(&mut buf).await {
                        Ok(n) if n > 0 => n,
                        _ => return,
                    };
                    let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let first_line = req.lines().next().unwrap_or("").to_string();
                    reqs.lock().await.push(first_line.clone());
                    // Reply 200 OK with body `{"method":..., "path":..., "host":...}`
                    let response = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 42\r\nConnection: close\r\n\r\n{\"method\":\"GET\",\"path\":\"/api\",\"host\":\"x\"}";
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        Self {
            addr,
            requests,
            handle,
        }
    }

    fn addr(&self) -> &str {
        &self.addr
    }
}

impl Drop for MockBackend {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

// ---------------------------------------------------------------------------
// DB seed helpers.
// ---------------------------------------------------------------------------

fn seed_site(conn: &Connection, name: &str, backend: &str) {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO sites (name, backend, enabled, created_at, updated_at) VALUES (?1, ?2, 1, ?3, ?3)",
        rusqlite::params![name, backend, now],
    )
    .expect("insert site");
}

fn seed_domain(conn: &Connection, domain: &str, site_name: &str) {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO domains (domain, site_name, enabled, created_at) VALUES (?1, ?2, 1, ?3)",
        rusqlite::params![domain, site_name, now],
    )
    .expect("insert domain");
}

fn seed_tun(conn: &Connection, name: &str, enabled: bool) {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO tun (name, enabled, online, registered_at, last_seen_at) VALUES (?1, ?2, 0, ?3, ?3)",
        rusqlite::params![name, enabled as i32, now],
    )
    .expect("insert tun");
}

fn seed_token(conn: &Connection, token: &str, enabled: bool, expires_at: Option<&str>) {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO tokens (token, enabled, created_at, expires_at) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![token, enabled as i32, now, expires_at],
    )
    .expect("insert token");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Smoke test: a freshly-spawned `pangolin-ngx` admin API responds
/// to `GET /api/sites` with the empty list.
#[tokio::test]
async fn real_e2e_admin_endpoint() {
    let ngx = NgxProcess::start(|db_path| {
        init_pangolin_db(db_path);
    })
    .await;

    let url = ngx.admin_url("/api/sites");
    let resp = reqwest::get(&url).await.expect("GET /api/sites");
    assert_eq!(
        resp.status(),
        200,
        "admin returned non-200: {}",
        ngx.log_string()
    );
    let body: serde_json::Value = resp.json().await.expect("parse JSON");
    assert!(body.is_array(), "expected array, got: {}", body);
    assert_eq!(body.as_array().unwrap().len(), 0, "expected empty sites");
}

/// `file:///` backend serves a static file. Pre-seeds a `data/static/index.html`
/// and a site/domain mapping to it, then GETs via the public proxy.
#[tokio::test]
async fn real_e2e_static_file() {
    let ngx = NgxProcess::start(|db_path| {
        init_pangolin_db(db_path);
        let backend = format!("file://{}/data/static", db_path.parent().unwrap().display());
        let conn = Connection::open(db_path).expect("open db");
        seed_site(&conn, "static-site", &backend);
        seed_domain(&conn, "static.example.com", "static-site");

        let static_dir = db_path.parent().unwrap().join("data").join("static");
        std::fs::create_dir_all(&static_dir).expect("mkdir static");
        let index_path = static_dir.join("index.html");
        std::fs::write(&index_path, "hello world").expect("write index.html");
    })
    .await;

    // URL is always `127.0.0.1:port` (the harness refuses to put a
    // fake host in the URL because reqwest would DNS-resolve it to
    // some public IP); the proxy routes by the `Host` header.
    let addr = format!("127.0.0.1:{}", ngx.http_port);
    let (status, body) = raw_get(&addr, "static.example.com", "/").await;
    assert_eq!(status, 200, "expected 200, got {}: {}", status, body);
    assert!(
        body.contains("hello world"),
        "expected 'hello world' in body, got: {} (binary log: {})",
        body,
        ngx.log_string()
    );
}

/// The "tunnel is wired up" test from CHECKLIST.md: a real
/// `pangolin-ngx` + real `pangolin-tun` with a valid token. The tun
/// performs the WebSocket upgrade + auth handshake, ngx marks the
/// tun online in the in-memory `tun_sessions` registry, and the
/// admin API can then list the online tun.
///
/// This deliberately does NOT exercise the full request-relay path
/// (proxy → tun → backend → response). That requires either a real
/// WebSocket backend (for the WS relay path) or a working HTTP
/// tunnel relay (which is a separate code path under active
/// development). Verifying connection + auth is the high-value
/// production check that no other test in this repo exercises.
#[tokio::test]
async fn real_e2e_tunnel_full() {
    let ngx = NgxProcess::start(|db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        seed_tun(&conn, "office", true);
        seed_token(&conn, "test-token", true, None);
    })
    .await;

    let tun = TunProcess::start(&ngx, "office", "test-token").await;
    let log = tun.log_string();
    assert!(
        log.contains("connected to ngx"),
        "tun did not complete WS upgrade + auth. log:\n{}",
        log
    );

    // Ask the admin API whether the tun is online. The expected
    // response is JSON like `{"name":"office","enabled":true,"online":true,...}`.
    let resp = reqwest::Client::new()
        .get(ngx.admin_url("/api/tun"))
        .send()
        .await
        .expect("GET /api/tun");
    assert_eq!(
        resp.status(),
        200,
        "admin /api/tun non-200: {}",
        ngx.log_string()
    );
    let body: serde_json::Value = resp.json().await.expect("parse JSON");
    let arr = body.as_array().expect("array response");
    assert_eq!(
        arr.len(),
        1,
        "expected 1 tun, got {}: {}",
        arr.len(),
        serde_json::to_string(&body).unwrap()
    );
    let entry = &arr[0];
    assert_eq!(entry["name"], "office");
    assert_eq!(entry["enabled"], true);
    assert_eq!(
        entry["online"], true,
        "expected tun online=true after connected handshake, got body: {}",
        entry
    );
}

/// Auth rejection: `pangolin-tun` connecting with an expired token.
/// The WS upgrade may complete (the server doesn't gate the upgrade
/// on auth), but ngx then resets the connection and the tun sees a
/// "Connection reset without closing handshake" or similar.
/// Importantly the tun should NOT see "tun office connected" (the
/// server-side log line emitted only after `validate_token` passes).
#[tokio::test]
async fn real_e2e_tunnel_token_rejected() {
    let ngx = NgxProcess::start(|db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        // tun is enabled, but the token is expired.
        seed_tun(&conn, "office", true);
        let past = (Utc::now() - ChronoDuration::hours(1)).to_rfc3339();
        seed_token(&conn, "expired-token", true, Some(&past.as_str()));
    })
    .await;

    let tun = TunProcess::start(&ngx, "office", "expired-token").await;
    let tun_log = tun.log_string();
    // The tun's "connected to ngx" line is logged on WS handshake
    // completion — that's before auth, so it MAY appear. What we
    // really want to assert is the SERVER side didn't log the
    // "tun office connected" line, which is gated on validate_token.
    let ngx_log = ngx.log_string();
    assert!(
        !ngx_log.contains("tun office connected"),
        "ngx accepted a tun with an expired token. ngx log:\n{}",
        ngx_log
    );
    // And the connection should be closed shortly after (within
    // a couple of seconds). The tun client may or may not log a
    // specific "auth failed" depending on its own error handling,
    // but it should NOT be happily reading frames indefinitely.
    assert!(
        tun_log.to_lowercase().contains("reset")
            || tun_log.to_lowercase().contains("disconnected")
            || tun_log.to_lowercase().contains("error")
            || tun_log.contains("reconnecting"),
        "expected tun to see connection reset/error/reconnect with bad token. log:\n{}",
        tun_log
    );
}
