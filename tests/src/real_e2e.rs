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
/// returning the response body. Wrapper around the shared
/// `harness::raw_request` — see that helper for why we bypass
/// reqwest (it ignores user-supplied `Host` headers in 0.12+).
async fn raw_get(addr: &str, host: &str, path: &str) -> (u16, String) {
    crate::harness::raw_request(addr, host, "GET", path, &[]).await
}

/// Issue a raw HTTP/1.1 request with a caller-chosen method.
async fn raw_request(
    addr: &str,
    host: &str,
    method: &str,
    path: &str,
    body: &[u8],
) -> (u16, String) {
    crate::harness::raw_request(addr, host, method, path, body).await
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
    // Convenience wrapper: insert a tun with token "test-token"
    // (matches the harness's `TunProcess::start(..., "test-token")`
    // call). v2: `tun` carries its own auth credential.
    seed_tun_with_token(conn, name, "test-token", enabled, None);
}

fn seed_tun_with_token(
    conn: &Connection,
    name: &str,
    token: &str,
    enabled: bool,
    expires_at: Option<&str>,
) {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO tun (name, token, enabled, online, registered_at, last_seen_at, expires_at)
         VALUES (?1, ?2, ?3, 0, ?4, ?4, ?5)",
        rusqlite::params![name, token, enabled as i32, now, expires_at],
    )
    .expect("insert tun");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Full HTTP request flow through the tunnel.
///
/// **This is the regression test for three bugs that the original
/// `real_e2e_tunnel_full` test missed**:
///
/// 1. The tunnel WS read loop in `tunnel.rs` was a `while let` that
///    consumed every message from the stream, leaving the inner
///    `select!` to read the *next* (non-existent) message — every
///    request was silently dropped. Fixed by switching to a flat
///    `loop { select! { msg = ws_read.next() => ... } }`.
///
/// 2. `proxy.rs` was sending a bare `TunnelRequestFrame` (msgpack),
///    but `tun::client::handle_stream` deserializes into `TunnelFrame`
///    and matches on the `Req` variant. The two types are NOT
///    wire-compatible: a serialized `TunnelRequestFrame` is not
///    decodable as a `TunnelFrame::Req`. Fixed by wrapping the frame
///    in `TunnelFrame::Req` before serializing.
///
/// 3. `proxy.rs` was sending `req.path` as the request line (e.g.
///    `/foo?bar`). The tun client then built the backend URL by
///    prepending the `Host` header — so a request to host
///    `yaitoo.cn` with a `tun local → http://127.0.0.1:9020`
///    backend ended up at `http://yaitoo.cn/foo?bar` (DNS failure
///    or wrong-server connection). Fixed by having ngx send the
///    full backend URL (e.g. `http://127.0.0.1:9020/foo?bar`) in
///    the `path` field.
///
/// This test catches all three by performing a real HTTP request
/// end-to-end: client → ngx → tun → backend. A regression on any
/// of the three bugs causes the request to hang/timeout.
#[tokio::test]
async fn real_e2e_tunnel_http_request_through_tun() {
    // Start a real mock HTTP backend that echoes the request method,
    // path, and Host header. We verify ALL THREE of (method, path,
    // Host) reach the backend unchanged, so a regression in URL
    // construction (Bug 3) is immediately visible.
    let backend = InspectingBackend::start().await;
    let backend_addr = backend.addr().to_string();

    let ngx = NgxProcess::start(move |db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        seed_tun(&conn, "office", true);
        // Backend format: `tun_name:url`. The `office` prefix tells
        // ngx to route through the "office" tun session. The URL is
        // the real destination — the tun client must use it as the
        // origin, NOT the Host header (Bug 3 regression check).
        seed_site(
            &conn,
            "office-site",
            &format!("office:http://{backend_addr}"),
        );
        seed_domain(&conn, "office.test", "office-site");
    })
    .await;

    // Spin up a real `pangolin-tun` binary. It connects to ngx's
    // tunnel port, performs the WS handshake + auth, and registers
    // itself in `app.tun_sessions["office"]`. After this returns,
    // every request to host `office.test` is supposed to flow
    // through this tun.
    let _tun = TunProcess::start(&ngx, "office", "test-token").await;

    // Make a real HTTP request to the proxy, with a path + query
    // and a method other than GET to make the assertions stronger.
    let addr = format!("127.0.0.1:{}", ngx.http_port);
    let (status, body) =
        raw_request(&addr, "office.test", "POST", "/api/echo?x=1&y=2", b"hello").await;

    // 1) Status must be 200 (not 502, not 504 timeout) — proves
    //    the request reached the backend and the response came
    //    back. A timeout here would mean the request was dropped
    //    (Bug 1) or malformed (Bug 2) on the wire.
    assert_eq!(
        status, 200,
        "expected 200 from backend via tunnel, got {status} (body={body:?}). \
         ngx log:\n{}\ntun log:\n(proxied)",
        ngx.log_string()
    );

    // 2) Backend must have actually received the request — proves
    //    ngx forwarded it through the tun to the configured
    //    backend, not to some default / wrong host (Bug 3).
    let seen = backend.seen().await;
    assert_eq!(
        seen.len(),
        1,
        "backend should have seen exactly 1 request, saw {}. \
         If 0, the request never reached the backend (Bug 3 — \
         wrong URL, or Bug 1/2 — request dropped before sending). \
         ngx log:\n{}",
        seen.len(),
        ngx.log_string()
    );
    let req = &seen[0];
    assert_eq!(req.method, "POST", "method must survive tunnel");
    assert_eq!(req.path, "/api/echo", "path part must survive tunnel");
    assert_eq!(
        req.query, "x=1&y=2",
        "query string must survive tunnel byte-exact"
    );
    assert_eq!(
        req.body, b"hello",
        "POST body must reach backend byte-exact through tunnel"
    );
    // Host header should be the public host (`office.test`) as
    // sent by the client — the proxy does NOT rewrite Host to
    // the backend's authority (it's a transparent forward proxy
    // for the Host header).
    let host_header = req
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Host"))
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    assert_eq!(
        host_header, "office.test",
        "Host header must pass through tunnel to backend as sent by client"
    );
}

/// `file:///` backend reached via the tunnel (not just direct).
///
/// Regression test for the static-file path through the tunnel.
/// With the new `tun::client::proxy_request` accepting `file:///`
/// URLs (in addition to `http://`/`https://`), a site configured
/// with `office:file:///tmp/somedir` must be served correctly when
/// the request goes through the tun.
///
/// Verifies:
///   - The tun decodes the `file:///` URL from the request frame
///   - It reads the local file and returns 200 with the body
///   - The body is returned to the client through the response frame
#[tokio::test]
async fn real_e2e_tunnel_file_backend() {
    // Build a temp directory holding a single file we want the
    // tun to serve. The path passed to seed_site must be the
    // **absolute** path (file:/// requires absolute).
    let static_dir = tempfile::Builder::new()
        .prefix("pangolin-e2e-tunnel-file-")
        .tempdir()
        .expect("tempdir for tunnel file backend");
    let index_path = static_dir.path().join("index.html");
    std::fs::write(&index_path, "tunnel-served-static-file")
        .expect("write index.html");
    let backend_url = format!("file://{}", static_dir.path().display());

    let ngx = NgxProcess::start(move |db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        seed_tun(&conn, "office", true);
        seed_site(&conn, "file-site", &format!("office:{backend_url}"));
        seed_domain(&conn, "file.test", "file-site");
    })
    .await;

    let _tun = TunProcess::start(&ngx, "office", "test-token").await;

    let addr = format!("127.0.0.1:{}", ngx.http_port);
    // Use POST with a body so the proxy's `read_body_or_idle`
    // path completes immediately (the GET-without-body case is a
    // separate `read_body_or_idle(false)` behavior that's still
    // being investigated). What we're testing here is the
    // file:// backend through the tunnel — the body is irrelevant
    // for that, we just need the path to flow through the tun.
    let (status, body) =
        raw_request(&addr, "file.test", "POST", "/index.html", b"x").await;
    assert_eq!(
        status, 200,
        "expected 200 for tunnel-served file, got {status} (body={body:?}). \
         ngx log:\n{}",
        ngx.log_string()
    );
    assert!(
        body.contains("tunnel-served-static-file"),
        "expected file body, got: {body} (ngx log: {})",
        ngx.log_string()
    );
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
    })
    .await;

    let tun = TunProcess::start(&ngx, "office", "test-token").await;
    let log = tun.log_string();
    assert!(
        log.contains("connected to ngx"),
        "tun did not complete WS upgrade + auth. log:\n{}",
        log
    );

    // Ask the admin UI whether the tun is online. Login first, then
    // GET the tunnels list and assert the row shows the online state.
    // The previous JSON-API-based check (`GET /api/tun`) was removed
    // along with the JSON API itself in the dashboard URL refactor
    // (see issue #31); the UI HTML page is now the only way to read
    // this state externally.
    let admin = crate::admin_harness::AdminClient::new(&ngx);
    admin.login("admin", "admin").await.expect("login");
    let resp = admin.get("/tun").await.expect("GET /tun");
    assert_eq!(
        resp.status().as_u16(),
        200,
        "admin /tun non-200: {}",
        ngx.log_string()
    );
    let body = resp.text().await.expect("read body");
    assert!(
        body.contains("office"),
        "tunnels page should list the 'office' tun, got: {}",
        body
    );
    assert!(
        body.contains("online"),
        "tunnels page should show 'office' as online after WS handshake, got: {}",
        body
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
        // v2: the tun row carries the token AND its expiry. An
        // expired token is now `expires_at` in the past on the
        // tun row.
        let past = (Utc::now() - ChronoDuration::hours(1)).to_rfc3339();
        seed_tun_with_token(&conn, "office", "expired-token", true, Some(&past.as_str()));
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

// ---------------------------------------------------------------------------
// Method-echoing mock backend for proxy forwarding tests.
// ---------------------------------------------------------------------------

/// A mock HTTP backend that replies 200 with a JSON body echoing
/// the incoming `method` and `path`. Used to verify that the
/// proxy forwards arbitrary HTTP methods + paths to the configured
/// upstream without filtering or rewriting.
struct EchoBackend {
    addr: String,
    requests: Arc<Mutex<Vec<(String, String)>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl EchoBackend {
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
                    let mut buf = vec![0u8; 8192];
                    let n = match stream.read(&mut buf).await {
                        Ok(n) if n > 0 => n,
                        _ => return,
                    };
                    let raw = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let first_line = raw.lines().next().unwrap_or("").to_string();
                    // first_line is e.g. "POST /api/foo HTTP/1.1"
                    let mut parts = first_line.split_whitespace();
                    let method = parts.next().unwrap_or("").to_string();
                    let path = parts.next().unwrap_or("").to_string();
                    reqs.lock().await.push((method.clone(), path.clone()));
                    let body = format!(
                        "{{\"method\":\"{}\",\"path\":\"{}\"}}",
                        method.replace('"', "\\\""),
                        path.replace('"', "\\\""),
                    );
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
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

    async fn seen(&self) -> Vec<(String, String)> {
        self.requests.lock().await.clone()
    }

    fn addr(&self) -> &str {
        &self.addr
    }
}

impl Drop for EchoBackend {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

// ---------------------------------------------------------------------------
// Inspecting mock backend: parses full request (method/path/query/headers/
// body) and returns a JSON echo.  Used by the per-feature forwarding tests
// below to make precise assertions on what the proxy actually delivered.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct RecordedRequest {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

struct InspectingBackend {
    addr: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl InspectingBackend {
    async fn start() -> Self {
        Self::start_with(200, "application/json", b"{}").await
    }

    async fn start_with(default_status: u16, default_ct: &str, default_body: &[u8]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let reqs_for_task = requests.clone();
        let default_ct = default_ct.to_string();
        let default_body = default_body.to_vec();
        let handle = tokio::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let reqs = reqs_for_task.clone();
                let default_ct = default_ct.clone();
                let default_body = default_body.clone();
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    // Read until we've seen the end-of-headers marker,
                    // then up to Content-Length more bytes for the body.
                    let mut accumulated = Vec::with_capacity(4096);
                    let mut tmp = [0u8; 1024];
                    let header_end = loop {
                        match stream.read(&mut tmp).await {
                            Ok(0) => return,
                            Ok(n) => {
                                accumulated.extend_from_slice(&tmp[..n]);
                                if let Some(pos) = find_double_crlf(&accumulated) {
                                    break pos;
                                }
                                if accumulated.len() > 64 * 1024 {
                                    return;
                                }
                            }
                            Err(_) => return,
                        }
                    };
                    let head_str = String::from_utf8_lossy(&accumulated[..header_end]).into_owned();
                    let mut lines = head_str.split("\r\n");
                    let request_line = lines.next().unwrap_or("");
                    let mut req_parts = request_line.split_whitespace();
                    let method = req_parts.next().unwrap_or("").to_string();
                    let full_path = req_parts.next().unwrap_or("").to_string();
                    let (path, query) = match full_path.find('?') {
                        Some(i) => (full_path[..i].to_string(), full_path[i + 1..].to_string()),
                        None => (full_path.clone(), String::new()),
                    };
                    let mut headers: Vec<(String, String)> = Vec::new();
                    let mut content_length: usize = 0;
                    for line in lines {
                        if line.is_empty() {
                            break;
                        }
                        if let Some((k, v)) = line.split_once(':') {
                            let k = k.trim().to_string();
                            let v = v.trim().to_string();
                            if k.eq_ignore_ascii_case("content-length") {
                                content_length = v.parse().unwrap_or(0);
                            }
                            headers.push((k, v));
                        }
                    }
                    // Now read the body.
                    let already_have = accumulated.len() - (header_end + 4);
                    let mut body = accumulated.split_off(header_end + 4);
                    while body.len() < content_length {
                        let need = content_length - body.len();
                        match stream.read(&mut tmp).await {
                            Ok(0) => break,
                            Ok(n) => body.extend_from_slice(&tmp[..n.min(need)]),
                            Err(_) => break,
                        }
                    }
                    reqs.lock().await.push(RecordedRequest {
                        method: method.clone(),
                        path: path.clone(),
                        query: query.clone(),
                        headers: headers.clone(),
                        body: body.clone(),
                    });
                    // Response — just an echo JSON for the default case.
                    let reason = match default_status {
                        200 => "OK",
                        201 => "Created",
                        204 => "No Content",
                        301 => "Moved Permanently",
                        302 => "Found",
                        400 => "Bad Request",
                        401 => "Unauthorized",
                        403 => "Forbidden",
                        404 => "Not Found",
                        500 => "Internal Server Error",
                        502 => "Bad Gateway",
                        _ => "Status",
                    };
                    let status_line = format!("HTTP/1.1 {} {}\r\n", default_status, reason);
                    let headers_out = format!(
                        "Content-Type: {ct}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n",
                        ct = default_ct,
                        len = default_body.len()
                    );
                    let _ = stream.write_all(status_line.as_bytes()).await;
                    let _ = stream.write_all(headers_out.as_bytes()).await;
                    let _ = stream.write_all(&default_body).await;
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

    async fn seen(&self) -> Vec<RecordedRequest> {
        self.requests.lock().await.clone()
    }
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

impl Drop for InspectingBackend {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Regression: the proxy must forward arbitrary HTTP methods to the
/// configured upstream without filtering.  The `/api/*` prefix is
/// reserved on the *admin* port (9081); on the *proxy* port (80) the
/// proxy must be transparent.  The previous implementation had
/// `AppProxy::request_filter` short-circuiting every `/api/*` path
/// into the admin API handler, which broke any backend whose public
/// API surface also lived under `/api/` (e.g. frtpilot's
/// `POST /api/channels/weixin/qr/start` returned 404 through the
/// proxy but 200 directly).
#[tokio::test]
async fn real_e2e_proxy_forwards_all_methods_on_api_path() {
    let backend = EchoBackend::start().await;

    let ngx = NgxProcess::start(|db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        seed_site(&conn, "echo-site", &format!("http://{}", backend.addr()));
        seed_domain(&conn, "echo.test", "echo-site");
    })
    .await;

    let addr = format!("127.0.0.1:{}", ngx.http_port);

    // All five core HTTP verbs, each on a /api/ path.  The proxy
    // must let every one through to the upstream.  We also cover
    // HEAD (cache validation, file-size probes) and OPTIONS (CORS
    // preflight, fired on every cross-origin fetch in modern web
    // apps) — the latter in particular was a real bug source for
    // the frtpilot integration: OPTIONS to /api/* would have been
    // swallowed by the admin handler pre-fix, breaking every
    // cross-origin call from the browser.
    let cases: &[(&str, &str)] = &[
        ("GET", "/api/foo"),
        ("POST", "/api/channels/weixin/qr/start"),
        ("PUT", "/api/items/42"),
        ("DELETE", "/api/items/42"),
        ("PATCH", "/api/items/42"),
        ("HEAD", "/api/foo"),
        ("OPTIONS", "/api/channels/weixin/qr/start"),
    ];

    for (method, path) in cases {
        let (status, body) = raw_request(&addr, "echo.test", method, path, b"").await;
        assert_eq!(
            status,
            200,
            "{method} {path} expected 200, got {status} (body={body:?}). ngx log:\n{}",
            ngx.log_string()
        );
        // Backend must have actually received the request (proves
        // the request reached the upstream, not just the proxy).
        // HEAD responses are required by RFC 9110 §15.3.2 to have
        // no body, so we skip the body assertion for that one.
        if *method != "HEAD" {
            let expected_body = format!("{{\"method\":\"{method}\",\"path\":\"{path}\"}}");
            assert_eq!(
                body, expected_body,
                "backend didn't echo back method+path correctly for {method} {path}"
            );
        }
    }

    // Sanity: the backend saw all requests, with the right
    // (method, path) pairs.
    let seen = backend.seen().await;
    assert_eq!(
        seen.len(),
        cases.len(),
        "backend expected to see {} requests, saw {}",
        cases.len(),
        seen.len()
    );
    for (i, (method, path)) in cases.iter().enumerate() {
        assert_eq!(seen[i].0, *method, "request {i} method mismatch");
        assert_eq!(seen[i].1, *path, "request {i} path mismatch");
    }
}

// ---------------------------------------------------------------------------
// Comprehensive proxy forwarding tests.
// ---------------------------------------------------------------------------

/// Request body must be forwarded byte-exact.  POST with a JSON
/// payload, PUT with a larger body — both must reach the upstream
/// unchanged.  A regression here would silently corrupt API calls.
#[tokio::test]
async fn real_e2e_proxy_forwards_request_body() {
    let backend = InspectingBackend::start().await;

    let ngx = NgxProcess::start(|db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        seed_site(&conn, "body-site", &format!("http://{}", backend.addr()));
        seed_domain(&conn, "body.test", "body-site");
    })
    .await;

    let addr = format!("127.0.0.1:{}", ngx.http_port);

    // Two payloads of different sizes (the second is larger than
    // the typical single-read buffer to verify Content-Length-driven
    // body read works).
    let small = br#"{"name":"alice","age":30}"#;
    let large = vec![b'X'; 64 * 1024]; // 64 KiB of 'X'

    for (method, body) in [("POST", &small[..]), ("PUT", &large[..])] {
        let (status, _resp_body) =
            raw_request(&addr, "body.test", method, "/api/items", body).await;
        assert_eq!(status, 200, "{method} with body expected 200, got {status}");
    }

    let seen = backend.seen().await;
    assert_eq!(
        seen.len(),
        2,
        "backend expected 2 requests, saw {}",
        seen.len()
    );
    assert_eq!(seen[0].method, "POST");
    assert_eq!(
        seen[0].body, small,
        "POST body must reach backend byte-exact"
    );
    assert_eq!(seen[1].method, "PUT");
    assert_eq!(
        seen[1].body, large,
        "PUT body must reach backend byte-exact"
    );
}

/// Query strings must survive the proxy intact.  Real APIs rely on
/// `?a=1&b=2` for filtering, pagination, search, etc.
#[tokio::test]
async fn real_e2e_proxy_preserves_query_string() {
    let backend = InspectingBackend::start().await;

    let ngx = NgxProcess::start(|db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        seed_site(&conn, "q-site", &format!("http://{}", backend.addr()));
        seed_domain(&conn, "q.test", "q-site");
    })
    .await;

    let addr = format!("127.0.0.1:{}", ngx.http_port);
    let (status, _) = raw_request(
        &addr,
        "q.test",
        "GET",
        "/api/items?limit=10&offset=20&q=hello%20world&tag=a%26b",
        b"",
    )
    .await;
    assert_eq!(status, 200);

    let seen = backend.seen().await;
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, "/api/items", "path part must be split off");
    assert_eq!(
        seen[0].query, "limit=10&offset=20&q=hello%20world&tag=a%26b",
        "query string must survive proxy byte-exact (incl. URL encoding and &)"
    );
}

/// The user's *original* suspicion for the frtpilot 404: cookies
/// and auth headers not surviving the proxy.  Verify both pass
/// through unchanged.
#[tokio::test]
async fn real_e2e_proxy_forwards_authorization_and_cookies() {
    let backend = InspectingBackend::start().await;

    let ngx = NgxProcess::start(|db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        seed_site(&conn, "auth-site", &format!("http://{}", backend.addr()));
        seed_domain(&conn, "auth.test", "auth-site");
    })
    .await;

    let addr = format!("127.0.0.1:{}", ngx.http_port);

    // Use a low-level raw request so we can set Cookie + Authorization
    // headers exactly.  raw_request doesn't take headers, so we
    // hand-write the wire bytes here.
    let mut stream = TcpStream::connect(&addr).await.expect("connect");
    let req = concat!(
        "GET /api/profile HTTP/1.1\r\n",
        "Host: auth.test\r\n",
        "Connection: close\r\n",
        "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.payload.signature\r\n",
        "Cookie: session=abc123; theme=dark\r\n",
        "X-Api-Key: sk-test-1234567890\r\n",
        "\r\n",
    );
    use tokio::io::AsyncWriteExt;
    stream.write_all(req.as_bytes()).await.expect("write");
    let mut buf = Vec::new();
    use tokio::io::AsyncReadExt;
    stream.read_to_end(&mut buf).await.expect("read");
    let _status_line = String::from_utf8_lossy(&buf[..buf.len().min(64)]).into_owned();

    let seen = backend.seen().await;
    assert_eq!(seen.len(), 1);

    // Pull specific headers from the recorded request and check
    // they were forwarded byte-exact.
    let h = |name: &str| -> String {
        seen[0]
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    assert_eq!(
        h("Authorization"),
        "Bearer eyJhbGciOiJIUzI1NiJ9.payload.signature",
        "Authorization must pass through byte-exact"
    );
    assert_eq!(
        h("Cookie"),
        "session=abc123; theme=dark",
        "Cookie header must pass through byte-exact"
    );
    assert_eq!(h("X-Api-Key"), "sk-test-1234567890");
}

/// The core feature of pangolin: the proxy routes by `Host` header.
/// Spin up TWO backends, register TWO domains, and verify that
/// requests to each domain land at the right backend (and not
/// the other one).
#[tokio::test]
async fn real_e2e_proxy_routes_hosts_to_different_backends() {
    let backend_a = InspectingBackend::start().await;
    let backend_b = InspectingBackend::start().await;

    let a_addr = backend_a.addr().to_string();
    let b_addr = backend_b.addr().to_string();

    let ngx = NgxProcess::start(move |db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        seed_site(&conn, "site-a", &format!("http://{a_addr}"));
        seed_domain(&conn, "a.example.com", "site-a");
        seed_site(&conn, "site-b", &format!("http://{b_addr}"));
        seed_domain(&conn, "b.example.com", "site-b");
    })
    .await;

    let addr = format!("127.0.0.1:{}", ngx.http_port);

    // Request to a.example.com → backend_a only.
    let (status_a, _) = raw_request(&addr, "a.example.com", "GET", "/", b"").await;
    assert_eq!(status_a, 200);
    // Request to b.example.com → backend_b only.
    let (status_b, _) = raw_request(&addr, "b.example.com", "GET", "/", b"").await;
    assert_eq!(status_b, 200);

    let seen_a = backend_a.seen().await;
    let seen_b = backend_b.seen().await;
    assert_eq!(seen_a.len(), 1, "backend_a should see exactly 1 request");
    assert_eq!(seen_b.len(), 1, "backend_b should see exactly 1 request");
    assert_eq!(seen_a[0].path, "/");
    assert_eq!(seen_b[0].path, "/");
}

/// 5xx and 4xx responses from the upstream must propagate to the
/// client unchanged.  A common bug is the proxy swallowing 5xx
/// and returning 200, which masks real backend failures.
#[tokio::test]
async fn real_e2e_proxy_propagates_backend_error_codes() {
    for (status, reason) in [
        (401, "Unauthorized"),
        (403, "Forbidden"),
        (500, "Internal Server Error"),
        (502, "Bad Gateway"),
        (404, "Not Found"),
    ] {
        let body = format!("{{\"error\":\"{reason}\"}}");
        let backend =
            InspectingBackend::start_with(status, "application/json", body.as_bytes()).await;

        let ngx = NgxProcess::start(|db_path| {
            init_pangolin_db(db_path);
            let conn = Connection::open(db_path).expect("open db");
            seed_site(&conn, "err-site", &format!("http://{}", backend.addr()));
            seed_domain(&conn, "err.test", "err-site");
        })
        .await;

        let addr = format!("127.0.0.1:{}", ngx.http_port);
        let (got_status, got_body) =
            raw_request(&addr, "err.test", "GET", "/api/whatever", b"").await;
        assert_eq!(
            got_status, status,
            "expected {status} from backend, got {got_status}"
        );
        assert_eq!(
            got_body, body,
            "expected backend body to be forwarded unchanged"
        );
    }
}

/// A reverse proxy should add `X-Forwarded-For` (and ideally
/// `X-Forwarded-Proto`) so the upstream can log the real client
/// IP.  If pangolin fails to add this, the upstream only sees
/// 127.0.0.1 (its local proxy peer) — log analysis becomes
/// useless.
///
/// **STATUS: known unimplemented feature.**  Grep finds zero
/// references to X-Forwarded-For / X-Real-IP / X-Forwarded-Proto
/// anywhere in `crates/`.  This test is `#[ignore]`'d so the rest
/// of the suite stays green, but the body stays runnable so the
/// future contributor who implements this can simply remove the
/// `#[ignore]` attribute and get a real regression check.
///
/// When implementing: the natural place is
/// `AppProxy::upstream_peer` in `crates/ngx/src/proxy.rs` —
/// modify the `HttpPeer` builder (or use pingora's
/// `proxy_set_header`-equivalent) to inject
/// `X-Forwarded-For: <client_ip>` before forwarding.
#[tokio::test]
#[ignore = "known unimplemented feature — see comment for implementation guidance"]
async fn real_e2e_proxy_adds_x_forwarded_for() {
    let backend = InspectingBackend::start().await;

    let ngx = NgxProcess::start(|db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        seed_site(&conn, "xff-site", &format!("http://{}", backend.addr()));
        seed_domain(&conn, "xff.test", "xff-site");
    })
    .await;

    let addr = format!("127.0.0.1:{}", ngx.http_port);
    // We don't control the local source IP from the test client
    // (it'll be 127.0.0.1 by definition), but we can verify the
    // header is PRESENT and contains a syntactically valid IP.
    let (status, _) = raw_request(&addr, "xff.test", "GET", "/api/x", b"").await;
    assert_eq!(status, 200);

    let seen = backend.seen().await;
    assert_eq!(seen.len(), 1);
    let xff = seen[0]
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("X-Forwarded-For"))
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    assert!(
        !xff.is_empty(),
        "X-Forwarded-For must be added by the proxy, but it was missing. headers: {:?}",
        seen[0].headers
    );
    // The header should contain at least one IP-looking token.
    assert!(
        xff.split(',')
            .any(|tok| tok.trim().parse::<std::net::IpAddr>().is_ok()),
        "X-Forwarded-For value must contain at least one parseable IP, got: {xff:?}"
    );
}

/// Response headers from the upstream must reach the client
/// unchanged.  Backend sets `X-Backend-Marker`, client must
/// receive it.  Important for things like `Set-Cookie`, CORS,
/// rate-limit headers, custom API headers.
#[tokio::test]
async fn real_e2e_proxy_forwards_response_headers() {
    // We need a backend that returns CUSTOM headers, not the
    // canned ones InspectingBackend sends.  Write a tiny one-off
    // backend here.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let handle = tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            // Drain the request, ignore it.
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf).await;
            let body = b"{\"ok\":true}";
            let response = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: {len}\r\n\
                 X-Backend-Marker: backend-says-hi\r\n\
                 X-RateLimit-Remaining: 42\r\n\
                 Set-Cookie: backend_cookie=abc; Path=/\r\n\
                 Connection: close\r\n\r\n",
                len = body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.write_all(body).await;
        }
    });

    let ngx = NgxProcess::start(move |db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        seed_site(&conn, "hdr-site", &format!("http://{addr}"));
        seed_domain(&conn, "hdr.test", "hdr-site");
    })
    .await;

    let proxy_addr = format!("127.0.0.1:{}", ngx.http_port);
    // Read the full response headers + body.
    let mut stream = TcpStream::connect(&proxy_addr).await.expect("connect");
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let req = "GET /api/x HTTP/1.1\r\nHost: hdr.test\r\nConnection: close\r\n\r\n";
    stream.write_all(req.as_bytes()).await.expect("write");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read");
    let response = String::from_utf8_lossy(&buf).into_owned();

    // Each marker header must appear verbatim (case-insensitive).
    assert!(
        response
            .to_lowercase()
            .contains("x-backend-marker: backend-says-hi"),
        "X-Backend-Marker must be forwarded, full response:\n{response}"
    );
    assert!(
        response
            .to_lowercase()
            .contains("x-ratelimit-remaining: 42"),
        "X-RateLimit-Remaining must be forwarded"
    );
    assert!(
        response
            .to_lowercase()
            .contains("set-cookie: backend_cookie=abc"),
        "Set-Cookie must be forwarded (this was part of the user's original concern)"
    );

    handle.abort();
}

/// CORS preflight (OPTIONS with `Origin` + `Access-Control-Request-*`
/// headers) must reach the upstream so it can answer with the right
/// CORS response headers.  If the proxy intercepts OPTIONS, every
/// cross-origin browser call fails.
#[tokio::test]
async fn real_e2e_proxy_forwards_cors_preflight() {
    // One-off backend that replies to OPTIONS with CORS headers.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let handle = tokio::spawn(async move {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(s) => s,
                Err(_) => return,
            };
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf).await;
                let first = String::from_utf8_lossy(&buf).into_owned();
                let is_options = first.starts_with("OPTIONS ");
                let body = b"";
                let (status, reason) = if is_options {
                    ("204", "No Content")
                } else {
                    ("200", "OK")
                };
                let cors_headers = if is_options {
                    "Access-Control-Allow-Origin: https://app.example.com\r\n\
                     Access-Control-Allow-Methods: GET, POST, PUT, DELETE, PATCH, OPTIONS\r\n\
                     Access-Control-Allow-Headers: Content-Type, Authorization, X-Api-Key\r\n\
                     Access-Control-Max-Age: 86400\r\n"
                } else {
                    "Access-Control-Allow-Origin: https://app.example.com\r\n"
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\n{cors}Content-Length: {len}\r\nConnection: close\r\n\r\n",
                    cors = cors_headers,
                    len = body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });

    let ngx = NgxProcess::start(move |db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        seed_site(&conn, "cors-site", &format!("http://{addr}"));
        seed_domain(&conn, "cors.test", "cors-site");
    })
    .await;

    let proxy_addr = format!("127.0.0.1:{}", ngx.http_port);
    let mut stream = TcpStream::connect(&proxy_addr).await.expect("connect");
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let req = "OPTIONS /api/widgets HTTP/1.1\r\n\
               Host: cors.test\r\n\
               Connection: close\r\n\
               Origin: https://app.example.com\r\n\
               Access-Control-Request-Method: POST\r\n\
               Access-Control-Request-Headers: Content-Type, Authorization\r\n\r\n";
    stream.write_all(req.as_bytes()).await.expect("write");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read");
    let response = String::from_utf8_lossy(&buf).into_owned().to_lowercase();

    assert!(
        response.contains("204 no content"),
        "OPTIONS preflight should get 204, got:\n{response}"
    );
    assert!(
        response.contains("access-control-allow-origin: https://app.example.com"),
        "CORS Allow-Origin from upstream must be forwarded"
    );
    assert!(
        response.contains("access-control-allow-methods:"),
        "CORS Allow-Methods must be forwarded"
    );
    assert!(
        response.contains("access-control-allow-headers:")
            && response.contains("authorization")
            && response.contains("x-api-key"),
        "CORS Allow-Headers must be forwarded with all requested headers"
    );

    handle.abort();
}

/// When the configured upstream is unreachable (port closed /
/// connection refused), the proxy must return 502 Bad Gateway
/// rather than 200 (with empty body) or 500 (with a stack trace).
/// 502 is the standard "upstream gone" response code.
#[tokio::test]
async fn real_e2e_proxy_returns_502_when_backend_unreachable() {
    // Bind a listener, capture its port, immediately drop the
    // listener so the port is closed.  The proxy will then get
    // ECONNREFUSED when it tries to dial the configured backend.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_addr = listener.local_addr().unwrap().to_string();
    drop(listener);

    let ngx = NgxProcess::start(move |db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        seed_site(&conn, "dead-site", &format!("http://{dead_addr}"));
        seed_domain(&conn, "dead.test", "dead-site");
    })
    .await;

    let proxy_addr = format!("127.0.0.1:{}", ngx.http_port);
    let (status, _body) = raw_request(&proxy_addr, "dead.test", "GET", "/api/foo", b"").await;
    assert_eq!(
        status,
        502,
        "expected 502 Bad Gateway when upstream is unreachable, got {status}. ngx log:\n{}",
        ngx.log_string()
    );
}

/// Regression test for the HTTP/2 host-lookup fix.
///
/// HTTP/2 clients don't send the `Host` header — the equivalent
/// is the `:authority` pseudo-header, which the proxy must fall
/// back to when looking up the site. Without that fallback the
/// proxy returns 404 for every H2 request, even when the SNI and
/// the backend route are otherwise correct.
///
/// Strategy:
/// 1. Generate a fresh self-signed ECDSA cert (won't expire, no
///    dependency on the workspace's `yaitoo.cn` Let's Encrypt
///    cert which would expire and break this test later).
/// 2. Seed a site mapping for the cert's CN and start the real
///    `pangolin-ngx` binary with the cert blob in place.
/// 3. `curl --http2 --http2-prior-knowledge` over TLS+SNI so the
///    request goes out as native H2, exercising the
///    `host_from_session` fallback path.
/// 4. Assert the response status + body match what the backend
///    returned, proving the proxy routed by `:authority`, not by
///    an empty `Host`.
#[tokio::test]
async fn real_e2e_h2_authority_fallback() {
    use std::process::Stdio;

    // Backend that responds 200 OK with a known marker body.
    let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = backend_listener.local_addr().unwrap().to_string();
    let backend_handle = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        if let Ok((mut stream, _)) = backend_listener.accept().await {
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
            let body = b"h2-authority-fallback";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes()).await;
            let _ = stream.write_all(body).await;
        }
    });

    let ngx = NgxProcess::start({
        let backend_addr = backend_addr.clone();
        move |db_path| {
            init_pangolin_db(db_path);
            let conn = Connection::open(db_path).expect("open db");
            seed_site(&conn, "h2-site", &format!("http://{backend_addr}"));
            // Domain matches the SNI we'll use in the curl request.
            seed_domain(&conn, "h2test.local", "h2-site");
        }
    })
    .await;

    // Generate a fresh self-signed ECDSA cert for `h2test.local`.
    // Using ECDSA (not the harness's RSA default) because some
    // local LibreSSL/openssl builds reject the rcgen RSA cert
    // (an unrelated toolchain quirk that we sidestep by using
    // the same ECDSA algorithm path that the production cert
    // pipeline uses). Generated fresh per run → never expires.
    let cert_dir = ngx.cert_dir();
    let cert_path = cert_dir.join("h2test.local");
    let status = std::process::Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:prime256v1",
            "-nodes",
            "-keyout",
            "/tmp/h2test.key",
            "-out",
            "/tmp/h2test.crt",
            "-days",
            "36500",
            "-subj",
            "/CN=h2test.local",
        ])
        .status()
        .expect("spawn openssl to generate cert");
    assert!(status.success(), "openssl cert generation failed");
    // Convert to the autocert DirCache blob layout: key PEM
    // first, then cert chain. The TLS callback's `split_blob`
    // requires this order.
    let key = std::fs::read_to_string("/tmp/h2test.key").expect("read key");
    let crt = std::fs::read_to_string("/tmp/h2test.crt").expect("read crt");
    std::fs::write(
        &cert_path,
        format!("{}\n{}", key.trim_end(), crt.trim_end()),
    )
    .expect("write cert blob");
    eprintln!(
        "h2 test setup: cert_path={} size={}",
        cert_path.display(),
        std::fs::metadata(&cert_path).map(|m| m.len()).unwrap_or(0)
    );

    // `curl --http2 --http2-prior-knowledge` to actually exercise
    // the H2 client framing. `--resolve` overrides DNS so we
    // don't write to /etc/hosts. `--insecure` skips cert
    // verification (the cert is self-signed; the test is about
    // routing, not TLS).
    //
    // `env_remove` strips HTTPS_PROXY/HTTP_PROXY so the user's
    // outbound proxy env vars don't intercept the connection.
    let url = format!("https://h2test.local:{}/path", ngx.tls_port);
    let output = tokio::process::Command::new("curl")
        .arg("--http2")
        .arg("--http2-prior-knowledge") // skip H1→H2 upgrade
        .arg("--insecure")
        .arg("--max-time")
        .arg("5")
        .arg("--silent")
        .arg("--show-error")
        .arg("--output")
        .arg("-")
        .arg("--write-out")
        .arg("HTTP_CODE:%{http_code}\n")
        .arg("--resolve")
        .arg(format!("h2test.local:{}:127.0.0.1", ngx.tls_port))
        .arg(&url)
        .env_remove("HTTPS_PROXY")
        .env_remove("https_proxy")
        .env_remove("HTTP_PROXY")
        .env_remove("http_proxy")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .expect("spawn curl");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // curl's `--write-out` line and the response body are both
    // written to stdout, with no separator, so the combined
    // string looks like `h2-authority-fallbackHTTP_CODE:200`.
    // Split on the marker; everything before is the body,
    // everything from the marker to the end is the curl metadata.
    let (body, meta) = stdout
        .split_once("HTTP_CODE:")
        .unwrap_or_else(|| {
            panic!(
                "curl did not emit HTTP_CODE marker. exit={:?}\nstdout: {stdout}\nstderr: {stderr}\nngx log:\n{}",
                output.status,
                ngx.log_string()
            )
        });
    let status: u16 = meta
        .trim()
        .lines()
        .next()
        .unwrap_or("0")
        .parse()
        .expect("parse http_code");
    let body = body.to_string();
    assert_eq!(
        status, 200,
        "expected 200 OK from backend via H2, got {status}.\nbody: {stdout}\nstderr: {stderr}\nngx log:\n{}",
        ngx.log_string()
    );
    assert!(
        stdout.contains("h2-authority-fallback"),
        "expected backend marker body, got: {stdout}\nngx log:\n{}",
        ngx.log_string()
    );

    backend_handle.abort();
}
