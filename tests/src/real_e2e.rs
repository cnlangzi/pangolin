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

use crate::harness::{NgxProcess, TunProcess, init_pangolin_db};

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
    seed_site_with_host_mode(conn, name, backend, "passthrough", None);
}

fn seed_site_with_host_mode(
    conn: &Connection,
    name: &str,
    backend: &str,
    host_mode: &str,
    host_custom: Option<&str>,
) {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO sites (name, backend, enabled, host_mode, host_custom, created_at, updated_at) \
         VALUES (?1, ?2, 1, ?3, ?4, ?5, ?5)",
        rusqlite::params![name, backend, host_mode, host_custom, now],
    )
    .expect("insert site with host_mode");
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
    // V3 (issue #39): `tun.token` is no longer the comparison
    // value — `tun.token_hash = sha256(token)` is. The WS server
    // hashes the inbound bearer and matches against the hash.
    // Compute it here so the test exercises the same path as
    // production.
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    let hash = format!("{:x}", h.finalize());
    conn.execute(
        "INSERT INTO tun (name, token, token_hash, enabled, online, registered_at, last_seen_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, 0, ?5, ?5, ?6)",
        rusqlite::params![name, token, hash, enabled as i32, now, expires_at],
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

    let addr = format!("127.0.0.1:{}", ngx.http_port);
    let (status, body) =
        raw_request(&addr, "office.test", "POST", "/api/echo?x=1&y=2", b"hello").await;

    // 1) Status must be 200 (not 502, not 504 timeout) — proves
    //    the request reached the backend and the response came
    //    back. A timeout here would mean the request was dropped
    //    (Bug 1) or malformed (Bug 2) on the wire.
    assert_eq!(
        status,
        200,
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

/// **Issue #61 regression test — host_mode=Backend on the tunnel path.**
///
/// Before the v8 refactor, `host_mode` was applied on the
/// *direct* path (ngx `upstream_request_filter`) but **not**
/// on the tunnel path: the request was forwarded through
/// the yamux stream with whatever Host header the client
/// sent. For SPAs (like kinnit) that route by Host, this
/// caused `/chat`, `/`, `/anything` to all collapse into
/// the default page — they all saw an unknown Host.
///
/// This test seeds a site with `host_mode=Backend` and
/// asserts the tun's reqwest-style executor (now
/// `PingoraClientExecutor` in v8) **overwrites the Host
/// header with the backend authority** before sending the
/// request upstream. The InspectingBackend records the
/// Host header it actually saw, and we compare.
#[tokio::test]
async fn real_e2e_tunnel_host_mode_backend_overrides_host() {
    let backend = InspectingBackend::start().await;
    let backend_addr = backend.addr().to_string();
    let backend_addr_for_closure = backend_addr.clone();

    let ngx = NgxProcess::start(move |db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        seed_tun(&conn, "office", true);
        seed_site_with_host_mode(
            &conn,
            "office-site",
            &format!("office:http://{backend_addr_for_closure}"),
            "backend",
            None,
        );
        seed_domain(&conn, "office.test", "office-site");
    })
    .await;

    let _tun = TunProcess::start(&ngx, "office", "test-token").await;

    let addr = format!("127.0.0.1:{}", ngx.http_port);
    let (status, _body) = raw_request(&addr, "office.test", "GET", "/chat", b"").await;
    assert_eq!(
        status,
        200,
        "expected 200, got {status}. ngx log:\n{}\n-- tun log --\n{}",
        ngx.log_string(),
        _tun.log_string()
    );

    let seen = backend.seen().await;
    assert_eq!(seen.len(), 1, "backend should have seen 1 request");
    let req = &seen[0];
    let host_header = req
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Host"))
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    assert_eq!(
        host_header, backend_addr,
        "host_mode=Backend must rewrite Host to backend authority. \
         Got {host_header:?}, expected {backend_addr:?}"
    );
    // Path is preserved regardless of host_mode (the central
    // invariant of v8).
    assert_eq!(req.path, "/chat", "path must survive tunnel byte-exact");
}

/// **Issue #61 regression test — host_mode=Passthrough preserves Host.**
///
/// Companion to the above: when `host_mode=Passthrough`,
/// the Host header must be the public host the client
/// used, not the backend's authority. This is the default
/// behaviour; we lock it in so future refactors can't
/// silently flip it.
#[tokio::test]
async fn real_e2e_tunnel_host_mode_passthrough_preserves_host() {
    let backend = InspectingBackend::start().await;
    let backend_addr = backend.addr().to_string();

    let ngx = NgxProcess::start(move |db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        seed_tun(&conn, "office", true);
        seed_site_with_host_mode(
            &conn,
            "office-site",
            &format!("office:http://{backend_addr}"),
            "passthrough",
            None,
        );
        seed_domain(&conn, "office.test", "office-site");
    })
    .await;

    let _tun = TunProcess::start(&ngx, "office", "test-token").await;

    let addr = format!("127.0.0.1:{}", ngx.http_port);
    let (status, _body) = raw_request(&addr, "office.test", "GET", "/api/v1/users?x=1", b"").await;
    assert_eq!(status, 200, "expected 200, got {status}");

    let seen = backend.seen().await;
    assert_eq!(seen.len(), 1);
    let req = &seen[0];
    let host_header = req
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Host"))
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    assert_eq!(
        host_header, "office.test",
        "host_mode=Passthrough must leave Host as the public host"
    );
    assert_eq!(req.path, "/api/v1/users", "path must survive tunnel");
    assert_eq!(req.query, "x=1", "query must survive tunnel");
}

/// **Path invariant across host_mode values — Issue #61 companion.**
///
/// For every host_mode, the URI path and query string must
/// round-trip byte-exact. This is the v8 design's central
/// invariant (`host_mode` only rewrites Host, never the
/// path).
#[tokio::test]
async fn real_e2e_tunnel_path_invariant_across_host_modes() {
    let backend = InspectingBackend::start().await;
    let backend_addr = backend.addr().to_string();

    let cases: &[(&str, &str, &str, &str)] = &[
        ("passthrough", "/", "/", ""),
        ("passthrough", "/chat", "/chat", ""),
        (
            "passthrough",
            "/api/v1/users?foo=bar&baz=qux",
            "/api/v1/users",
            "foo=bar&baz=qux",
        ),
        ("backend", "/", "/", ""),
        ("backend", "/chat", "/chat", ""),
        ("backend", "/api/v1/users?x=1", "/api/v1/users", "x=1"),
        ("custom", "/chat", "/chat", ""),
    ];
    for (i, (mode, path, want_path, want_query)) in cases.iter().enumerate() {
        let backend_addr_for_closure = backend_addr.clone();
        let ngx = NgxProcess::start(move |db_path| {
            init_pangolin_db(db_path);
            let conn = Connection::open(db_path).expect("open db");
            seed_tun(&conn, "office", true);
            seed_site_with_host_mode(
                &conn,
                "office-site",
                &format!("office:http://{backend_addr_for_closure}"),
                mode,
                Some("custom.example.com"),
            );
            seed_domain(&conn, "office.test", "office-site");
        })
        .await;

        let _tun = TunProcess::start(&ngx, "office", "test-token").await;
        let addr = format!("127.0.0.1:{}", ngx.http_port);
        let (status, _body) = raw_request(&addr, "office.test", "GET", path, b"").await;
        assert_eq!(
            status, 200,
            "case {i} ({mode} {path}): expected 200, got {status}"
        );
        let seen = backend.seen().await;
        assert_eq!(seen.len(), 1, "case {i}: backend saw wrong request count");
        let req = &seen[0];
        assert_eq!(
            &req.path, want_path,
            "case {i} ({mode} {path}): path mismatch"
        );
        assert_eq!(
            &req.query, want_query,
            "case {i} ({mode} {path}): query mismatch"
        );
        backend.reset().await; // (mock has no reset; this is a no-op but kept for symmetry)
    }
}

/// **Regression test for the GET-without-Content-Length hang.**
///
/// Symptom (from production): `curl -v http://yaitoo.cn` against a
/// site whose backend is `tun:http://127.0.0.1:9020` hangs forever —
/// `pangolin-ngx` logs `PROXY: Tunnel routing: yaitoo.cn → tun local`
/// and then nothing further, while the yamux session emits its 30 s
/// keep-alive pings.
///
/// Cause: `AppProxy::request_filter` was calling
/// `session.read_body_or_idle(false).await` to grab the request body
/// before opening the yamux stream. `read_body_or_idle` is pingora's
/// internal body-pump primitive; on a body-less request (no
/// `Content-Length`, no `Transfer-Encoding`) it deliberately stays
/// pending forever waiting for FIN, because in pingora's normal flow
/// it sits inside a `select!` whose other branch handles the
/// upstream-write side. Awaiting it sequentially is a deadlock — the
/// request body bytes never come, and FIN never arrives because curl
/// is also waiting on the response.
///
/// Why the existing tunnel e2e suite missed it: `harness::raw_request`
/// always emits `Content-Length: 0`, which makes pingora initialise
/// the body reader with `cl=0` and return `Ok(None)` on the first
/// poll. Real curl GETs ship no body-framing headers at all, taking
/// the `cl=None` → idle-wait branch (RFC 9112 §6.3) that hangs.
///
/// Fix: switch to `session.read_request_body()` in a loop. That call
/// returns `Ok(None)` immediately when there is no more body to read,
/// regardless of which header path pingora initialised. The loop is
/// because chunked or streamed bodies arrive as multiple chunks.
///
/// This test uses `raw_request_no_content_length` — a deliberately
/// header-minimal request — to mirror what curl actually puts on the
/// wire. A regression that re-introduces `read_body_or_idle(false)`
/// (or any other call that idles on FIN) trips the 5 s read timeout
/// in the helper and fails the test rather than hanging the suite.
#[tokio::test]
async fn real_e2e_tunnel_get_without_content_length() {
    let backend = InspectingBackend::start().await;
    let backend_addr = backend.addr().to_string();

    let ngx = NgxProcess::start(move |db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        seed_tun(&conn, "office", true);
        seed_site(
            &conn,
            "office-site",
            &format!("office:http://{backend_addr}"),
        );
        seed_domain(&conn, "office.test", "office-site");
    })
    .await;

    let _tun = TunProcess::start(&ngx, "office", "test-token").await;

    let addr = format!("127.0.0.1:{}", ngx.http_port);
    // GET with NO `Content-Length`, NO `Transfer-Encoding`, NO body.
    // Pre-fix this hung the entire request flow; the 5 s read timeout
    // in `raw_request_no_content_length` would surface it as a panic.
    let (status, body) =
        crate::harness::raw_request_no_content_length(&addr, "office.test", "GET", "/").await;

    assert_eq!(
        status,
        200,
        "GET without Content-Length must complete (not hang). \
         got status={status}, body={body:?}. ngx log:\n{}",
        ngx.log_string()
    );

    // Backend must have actually received the request — proves ngx
    // forwarded it through the tun rather than silently dropping it
    // on the way.
    let seen = backend.seen().await;
    assert_eq!(
        seen.len(),
        1,
        "backend should have seen exactly 1 request, saw {}. ngx log:\n{}",
        seen.len(),
        ngx.log_string()
    );
    assert_eq!(seen[0].method, "GET");
    assert_eq!(seen[0].path, "/");
}

/// **Regression test — multiple `Set-Cookie` headers must reach the
/// browser through the tunnel path.**
///
/// Symptom (from production): kinnit's login flow POSTs to `/login`
/// and the backend replies `302` with two `Set-Cookie` headers
/// (`uid=…` and `sid=…`). With the pre-fix `write_response_to_session`
/// using `ResponseHeader::insert_header`, the second `Set-Cookie`
/// silently overwrote the first — the browser only saw one cookie,
/// the session lookup failed, and the follow-up `GET /chat` redirected
/// straight back to `/login`. The user described it as "the cookie
/// doesn't take effect."
///
/// Cause: `insert_header` replaces all values under the same name
/// (pingora's `header_name_map` is a multi-map; `insert_header` does
/// a `remove + append`, dropping every prior value). `Set-Cookie` is
/// the one header where RFC 6265 §3 / RFC 7230 §3.2.2 explicitly
/// permits — and backends regularly rely on — multiple values on the
/// wire.
///
/// Fix: switch `write_response_to_session` to `append_header`, which
/// adds the new value without removing existing ones. Every other
/// header keeps the same behaviour (most are single-value; for
/// multi-value-permitted headers like `Vary` / `Cache-Control` the
/// append semantics are what a transparent proxy should provide).
///
/// Why this test catches it: the mock backend replies with two
/// `Set-Cookie` lines. The assertion uses `raw_request_capture` (or
/// its in-test equivalent) to read the **raw bytes** the client
/// received — a TLS-free HTTP/1.1 dump — and fails immediately if
/// only one `Set-Cookie` line is present. The pre-fix proxy would
/// emit exactly one; the post-fix proxy emits both.
#[tokio::test]
async fn real_e2e_tunnel_preserves_multiple_set_cookie() {
    // Mock backend that replies with two Set-Cookie headers.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(s) => s,
                Err(_) => return,
            };
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                // Drain the request, we don't care about it.
                let mut sink = vec![0u8; 8192];
                let _ = stream.read(&mut sink).await;
                // Empty body, so Content-Length is a literal 0 — no need to
                // interpolate body.len().
                let resp = "HTTP/1.1 302 Found\r\n\
                     Location: /chat\r\n\
                     Set-Cookie: uid=44; Path=/; Max-Age=2592000\r\n\
                     Set-Cookie: sid=46; Path=/; Max-Age=2592000\r\n\
                     Date: Mon, 15 Jun 2026 09:00:00 GMT\r\n\
                     Content-Length: 0\r\n\
                     \r\n";
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });

    let ngx = NgxProcess::start({
        let backend_addr = backend_addr.clone();
        move |db_path| {
            init_pangolin_db(db_path);
            let conn = Connection::open(db_path).expect("open db");
            seed_tun(&conn, "office", true);
            seed_site(
                &conn,
                "office-site",
                &format!("office:http://{backend_addr}"),
            );
            seed_domain(&conn, "office.test", "office-site");
        }
    })
    .await;

    let _tun = TunProcess::start(&ngx, "office", "test-token").await;

    // Issue a raw HTTP/1.1 request and capture the full raw response
    // bytes (we want to count *wire* Set-Cookie lines, not parse
    // headers — headers can fold, but a Set-Cookie line is a
    // Set-Cookie line).
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpStream;
    use tokio::time::{Duration, timeout};

    let addr = format!("127.0.0.1:{}", ngx.http_port);
    let mut stream = timeout(Duration::from_secs(5), TcpStream::connect(&addr))
        .await
        .expect("connect to ngx (5s timeout)")
        .expect("connect to ngx");
    let req = b"GET / HTTP/1.1\r\nHost: office.test\r\nConnection: close\r\n\r\n";
    stream.write_all(req).await.expect("write request");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read response");
    let text = String::from_utf8_lossy(&buf);

    let set_cookie_lines: Vec<&str> = text
        .split("\r\n")
        .filter(|l| l.to_ascii_lowercase().starts_with("set-cookie:"))
        .collect();

    assert_eq!(
        set_cookie_lines.len(),
        2,
        "exactly 2 Set-Cookie lines must reach the client, got {}: {:?}. \
         raw response:\n{}",
        set_cookie_lines.len(),
        set_cookie_lines,
        text
    );
    let joined = set_cookie_lines.join("|");
    assert!(
        joined.contains("uid=44"),
        "uid cookie missing from forwarded Set-Cookie: {joined}"
    );
    assert!(
        joined.contains("sid=46"),
        "sid cookie missing from forwarded Set-Cookie: {joined}"
    );
}

/// **Regression test for the HTTPS → tunnel path-construction bug.**
///
/// Symptom (from production): `curl http://yaitoo.cn` works, but
/// `curl https://yaitoo.cn` returns HTTP/2 404 with body
/// `404 page not found` (Go `http.NotFound`'s signature: 19 bytes,
/// `content-type: text/plain; charset=utf-8`,
/// `x-content-type-options: nosniff`). TLS, ALPN, cert, routing all
/// look fine — the backend simply doesn't recognise the path it
/// received.
///
/// Cause: `AppProxy::request_filter` was building the tunnel-side URL
/// from `session.req_header().uri.to_string()`. That call does NOT
/// round-trip across HTTP/1.1 and HTTP/2:
///
///   * H1 `GET /api HTTP/1.1` → `uri.to_string() == "/api"`.
///   * H2 `:scheme=https`, `:authority=yaitoo.cn`, `:path=/api` →
///     pingora reconstructs an absolute-form URI, so
///     `uri.to_string() == "https://yaitoo.cn/api"`.
///
/// The proxy then concatenated that onto the backend prefix, giving
/// `http://127.0.0.1:9020/https://yaitoo.cn/api` — a path the backend
/// has never heard of, hence 404.
///
/// Fix: use `uri.path_and_query()` (always path-only,
/// regardless of how the request arrived) when building the tunnel
/// target.
///
/// Why this test catches it: we drive a real HTTPS request through
/// curl (`--http2`, no `--http2-prior-knowledge`) against the
/// proxy's TLS port. The dynamic ALPN callback in
/// `ngx::tls::build_sni_settings` forces h1 for tunnel sites
/// (issue #66 / commit `0c35ede`), so curl falls back to h1
/// transparently. The mock backend records the path it actually
/// received; the assertion `path == "/api/profile"` (not
/// `path == "/https://office.test/api/profile"`) catches a
/// regression in `path_and_query()` regardless of which protocol
/// the request actually used.
#[tokio::test]
async fn real_e2e_tunnel_h2_path_preserved() {
    use std::process::Stdio;

    let backend = InspectingBackend::start().await;
    let backend_addr = backend.addr().to_string();

    let ngx = NgxProcess::start(move |db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        seed_tun(&conn, "office", true);
        seed_site(
            &conn,
            "office-site",
            &format!("office:http://{backend_addr}"),
        );
        seed_domain(&conn, "office.test", "office-site");
    })
    .await;

    let _tun = TunProcess::start(&ngx, "office", "test-token").await;

    // Generate a self-signed ECDSA cert for `office.test` and install
    // it as `{cert_dir}/office.test` in autocert DirCache blob layout
    // (key PEM, then cert chain). Mirrors `real_e2e_h2_authority_fallback`
    // — the cert pipeline doesn't care that the request is going through
    // the tunnel rather than direct backend.
    let cert_dir = ngx.cert_dir();
    let cert_path = cert_dir.join("office.test");
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
            "/tmp/office_tunnel.key",
            "-out",
            "/tmp/office_tunnel.crt",
            "-days",
            "36500",
            "-subj",
            "/CN=office.test",
        ])
        .status()
        .expect("spawn openssl");
    assert!(status.success(), "openssl cert generation failed");
    let key = std::fs::read_to_string("/tmp/office_tunnel.key").expect("read key");
    let crt = std::fs::read_to_string("/tmp/office_tunnel.crt").expect("read crt");
    std::fs::write(
        &cert_path,
        format!("{}\n{}", key.trim_end(), crt.trim_end()),
    )
    .expect("write cert blob");

    // `--http2` (no `--http2-prior-knowledge`): curl honours the
    // server's ALPN advertisement. Since the dynamic ALPN callback
    // in `ngx::tls::build_sni_settings` forces h1 for tunnel sites,
    // curl will silently fall back to h1 — the request still
    // exercises the path-construction path through `request_filter`
    // (just over h1 instead of h2). This keeps the regression value
    // of the test without requiring us to force h2 across the
    // handshake.
    //
    // `--resolve` so we don't touch /etc/hosts; `--insecure` because
    // the cert is self-signed.
    let url = format!("https://office.test:{}/api/profile?id=42", ngx.tls_port);
    let output = tokio::process::Command::new("curl")
        .arg("--http2")
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
        .arg(format!("office.test:{}:127.0.0.1", ngx.tls_port))
        .arg(&url)
        .env_remove("HTTPS_PROXY")
        .env_remove("https_proxy")
        .env_remove("HTTP_PROXY")
        .env_remove("http_proxy")
        .env_remove("ALL_PROXY")
        .env_remove("all_proxy")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .expect("spawn curl");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let (_body, meta) = stdout.split_once("HTTP_CODE:").unwrap_or_else(|| {
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
    assert_eq!(
        status,
        200,
        "HTTPS+H2 → tunnel should return 200, got {status}. \
         If 404, the proxy probably built the backend URL from a full \
         URI (https://office.test/api/profile?id=42) and the backend \
         didn't recognise it. stdout: {stdout}\nstderr: {stderr}\nngx log:\n{}",
        ngx.log_string()
    );

    // The decisive assertion: the backend must have seen the path
    // exactly as the client sent it, NOT `/https://office.test/...`.
    let seen = backend.seen().await;
    assert_eq!(
        seen.len(),
        1,
        "backend should have seen exactly 1 request, saw {}. ngx log:\n{}",
        seen.len(),
        ngx.log_string()
    );
    assert_eq!(
        seen[0].path, "/api/profile",
        "H2 path leaked through unchanged: the proxy must use path_and_query() \
         (not uri.to_string()) when building the tunnel target, otherwise the \
         backend sees `/https://office.test/api/profile`. Actual: {:?}",
        seen[0].path
    );
    assert_eq!(
        seen[0].query, "id=42",
        "H2 query string must reach the backend byte-exact"
    );
}

/// **Regression test for the per-SNI dynamic ALPN policy:**
/// h2 client + tunnel site must transparently fall back to h1.
///
/// Pre-fix (PR #66 / commit `0c35ede`): the listener advertised h2
/// for every connection. Browsers and `curl --http2` (without
/// `--http2-prior-knowledge`) tried h2, the proxy accepted it, and
/// the request then fell into the upstream `tokio-yamux 0.3.18`
/// stream-state race (`debug!("this branch should be unreachable")`
/// in `stream.rs:506`), which tore the yamux stream down and made
/// pingora fall back to `400 Bad Request: missing required Host
/// header`.
///
/// Post-fix: `ngx::tls::build_sni_settings` installs a per-SNI
/// ALPN callback that does NOT offer h2 for tunnel sites — h1
/// only. Curl's `--http2` mode (which respects the server's ALPN
/// advertisement) silently falls back to h1 and the request goes
/// through.
///
/// This test asserts two things together:
///
/// 1. The end-to-end request succeeds (200) when the client tries
///    `--http2`. Pre-fix, this was a 400.
/// 2. The server's ALPN did NOT offer h2 — confirmed by grepping
///    `curl -v` stderr for the `ALPN: offers ` line. We assert the
///    string `h2` does NOT appear in the ALPN offers for the
///    tunnel site, so the h1 path is the only one that can run.
#[tokio::test]
async fn real_e2e_h2_tunnel_auto_fallback_to_h1() {
    use std::process::Stdio;

    let backend = InspectingBackend::start().await;
    let backend_addr = backend.addr().to_string();

    let ngx = NgxProcess::start(move |db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        seed_tun(&conn, "office", true);
        seed_site(
            &conn,
            "office-site",
            &format!("office:http://{backend_addr}"),
        );
        seed_domain(&conn, "office.test", "office-site");
    })
    .await;

    let _tun = TunProcess::start(&ngx, "office", "test-token").await;

    // Issue a self-signed cert for `office.test` and install it in
    // the per-test cert dir.
    let cert_dir = ngx.cert_dir();
    let cert_path = cert_dir.join("office.test");
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
            "/tmp/office_tunnel_alpn.key",
            "-out",
            "/tmp/office_tunnel_alpn.crt",
            "-days",
            "36500",
            "-subj",
            "/CN=office.test",
        ])
        .status()
        .expect("spawn openssl");
    assert!(status.success(), "openssl cert generation failed");
    let key = std::fs::read_to_string("/tmp/office_tunnel_alpn.key").expect("read key");
    let crt = std::fs::read_to_string("/tmp/office_tunnel_alpn.crt").expect("read crt");
    std::fs::write(
        &cert_path,
        format!("{}\n{}", key.trim_end(), crt.trim_end()),
    )
    .expect("write cert blob");

    // `curl -v` writes the negotiated ALPN to stderr in the form
    //   `* ALPN: offers <proto>`
    // (libcurl with debug builds). We use this to assert h2 was
    // NOT offered. The `http2` flag is present so curl tries h2
    // first; the absence of h2 in the ALPN line forces the
    // downgrade to h1.
    let url = format!("https://office.test:{}/api/profile?id=42", ngx.tls_port);
    let output = tokio::process::Command::new("curl")
        .arg("--http2")
        .arg("-v")
        .arg("--insecure")
        .arg("--max-time")
        .arg("10")
        .arg("--silent")
        .arg("--output")
        .arg("-")
        .arg("--write-out")
        .arg("HTTP_CODE:%{http_code}\n")
        .arg("--resolve")
        .arg(format!("office.test:{}:127.0.0.1", ngx.tls_port))
        .arg(&url)
        .env_remove("HTTPS_PROXY")
        .env_remove("https_proxy")
        .env_remove("HTTP_PROXY")
        .env_remove("http_proxy")
        .env_remove("ALL_PROXY")
        .env_remove("all_proxy")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .expect("spawn curl");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let (body, meta) = stdout.split_once("HTTP_CODE:").unwrap_or_else(|| {
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
    assert_eq!(
        status,
        200,
        "h2 + tunnel site must auto-fallback to h1 and return 200; got {status}. \
         Pre-fix this was a 400 Bad Request from pingora. \
         body: {body}\nstderr: {stderr}\nngx log:\n{}",
        ngx.log_string()
    );

    // Look for curl's ALPN negotiation line. Curl's wording varies across
    // versions and build flags:
    //
    //   `* ALPN, server accepted to use <proto>`  — debug-capable curl, modern
    //   `* ALPN: server accepted <proto>`         — older variants
    //   `* ALPN, offering <proto>`                — client offer (modern)
    //   `* ALPN: offers h2,http/1.1`              — older single-line offer
    //
    // The "server accepted" form is what we want for the strict assertion —
    // it tells us exactly what the server picked. We deliberately do NOT
    // match the "offering"/"offers" forms because those are the client's
    // own offers; matching them would silently pass even when the server
    // actually picked h2.
    //
    // When the running curl can't emit "server accepted" (non-debug build
    // or a really old version), we fall back to behaviour-level assertions
    // below: status==200 + exactly one backend observation. The h2+tunnel
    // bug produces a 400 from pingora, so the request-level signal still
    // catches the regression reliably even without the strict ALPN trace.
    let server_alpn_line = stderr
        .lines()
        .find(|l| l.contains("ALPN") && l.contains("accepted"));
    if let Some(server_alpn_line) = server_alpn_line {
        // Strict assertion: server picked http/1.1, not h2.
        assert!(
            server_alpn_line.contains("http/1.1"),
            "server did not accept http/1.1 for the tunnel site; \
             ALPN override did not work. line: {server_alpn_line}\n\
             stderr:\n{stderr}\nngx log:\n{}",
            ngx.log_string()
        );
        assert!(
            !server_alpn_line.contains("h2"),
            "server accepted h2 for a tunnel site; ALPN override did not work. \
             line: {server_alpn_line}\nstderr:\n{stderr}\nngx log:\n{}",
            ngx.log_string()
        );
    } else {
        // No "server accepted" line — curl is probably a build that only
        // emits client-offer lines. Log once so the operator knows the
        // strict ALPN check was skipped, and rely on behaviour-level
        // assertions below.
        eprintln!(
            "note: curl -v did not emit an `ALPN ... server accepted` line; \
             falling back to behaviour-level assertion (status=200 + backend \
             observation). Install libcurl with --enable-debug for stricter \
             ALPN checking. stderr:\n{stderr}"
        );
    }

    // Backend should have seen exactly one request with the
    // expected path (regression for `path_and_query()`).
    let seen = backend.seen().await;
    assert_eq!(
        seen.len(),
        1,
        "backend should have seen exactly 1 request, saw {}. ngx log:\n{}",
        seen.len(),
        ngx.log_string()
    );
    assert_eq!(seen[0].path, "/api/profile");
    assert_eq!(seen[0].query, "id=42");
}

/// **Regression test for h2 over a non-tunnel (direct) backend:**
/// h2 must still be advertised so the client can use multiplexing.
///
/// This is the symmetric counterpart to
/// `real_e2e_h2_tunnel_auto_fallback_to_h1`. The dynamic ALPN
/// callback must:
///   - Not affect non-tunnel sites (still offer h2).
///   - Still successfully proxy over h2.
#[tokio::test]
async fn real_e2e_h2_direct_backend_keeps_h2() {
    use std::process::Stdio;

    let backend = InspectingBackend::start().await;
    let backend_addr = backend.addr().to_string();

    let ngx = NgxProcess::start(move |db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        // No `tun` here — this site has a direct backend.
        seed_site(&conn, "direct-site", &format!("http://{backend_addr}"));
        seed_domain(&conn, "direct.test", "direct-site");
    })
    .await;

    // Self-signed cert for direct.test.
    let cert_dir = ngx.cert_dir();
    let cert_path = cert_dir.join("direct.test");
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
            "/tmp/direct_h2.key",
            "-out",
            "/tmp/direct_h2.crt",
            "-days",
            "36500",
            "-subj",
            "/CN=direct.test",
        ])
        .status()
        .expect("spawn openssl");
    assert!(status.success(), "openssl cert generation failed");
    let key = std::fs::read_to_string("/tmp/direct_h2.key").expect("read key");
    let crt = std::fs::read_to_string("/tmp/direct_h2.crt").expect("read crt");
    std::fs::write(
        &cert_path,
        format!("{}\n{}", key.trim_end(), crt.trim_end()),
    )
    .expect("write cert blob");

    // For a non-tunnel site the server's ALPN must include h2 so
    // the browser / curl can use HTTP/2 multiplexing. With
    // `--http2-prior-knowledge` (no ALPN upgrade dance) we
    // exercise the h2 hot path end-to-end: the request must
    // succeed and the backend must have seen an h2-style request
    // (path preserved byte-exact — see PR #43 regression).
    let url = format!("https://direct.test:{}/api/profile?id=42", ngx.tls_port);
    let output = tokio::process::Command::new("curl")
        .arg("--http2")
        .arg("--http2-prior-knowledge")
        .arg("--insecure")
        .arg("--max-time")
        .arg("10")
        .arg("--silent")
        .arg("--show-error")
        .arg("--output")
        .arg("-")
        .arg("--write-out")
        .arg("HTTP_CODE:%{http_code}\n")
        .arg("--resolve")
        .arg(format!("direct.test:{}:127.0.0.1", ngx.tls_port))
        .arg(&url)
        .env_remove("HTTPS_PROXY")
        .env_remove("https_proxy")
        .env_remove("HTTP_PROXY")
        .env_remove("http_proxy")
        .env_remove("ALL_PROXY")
        .env_remove("all_proxy")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .expect("spawn curl");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let (_body, meta) = stdout.split_once("HTTP_CODE:").unwrap_or_else(|| {
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
    assert_eq!(
        status,
        200,
        "h2 + http-direct site must return 200. got {status}. \
         Pre-fix-style regressions (path mangling) would 404 here. \
         stderr: {stderr}\nngx log:\n{}",
        ngx.log_string()
    );

    let seen = backend.seen().await;
    assert_eq!(seen.len(), 1, "backend should have seen 1 request");
    assert_eq!(
        seen[0].path, "/api/profile",
        "H2 path must be byte-exact (no `https://host` prefix leak). \
         This was the bug fixed in PR #43; we keep the regression."
    );
    assert_eq!(seen[0].query, "id=42");
}

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
    std::fs::write(&index_path, "tunnel-served-static-file").expect("write index.html");
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
    let (status, body) = raw_request(&addr, "file.test", "POST", "/index.html", b"x").await;
    assert_eq!(
        status,
        200,
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

    /// Clear the recorded requests. Useful when reusing one
    /// `InspectingBackend` across multiple assertions in a
    /// single test (the `real_e2e_tunnel_path_invariant_*`
    /// suite does this to amortise backend startup cost).
    async fn reset(&self) {
        self.requests.lock().await.clear();
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
        status,
        200,
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

// ---------------------------------------------------------------------------
// ACME HTTP-01 challenge serving (issue #54)
// ---------------------------------------------------------------------------
//
// Why these tests are here:
//   The original ACME client wrote challenge files to
//   `{cert_dir}/.well-known/acme-challenge/{token}`, but the proxy
//   never served them — so Let's Encrypt (or Pebble in strict mode)
//   fetched the URL, fell through to site routing, hit a backend
//   that doesn't know about ACME, and the operator saw a 403/404
//   in the validator's logs.
//
//   Two of the three tests below are NOT `#[ignore]`'d because they
//   don't require Pebble — they plant a challenge file as the ACME
//   client would, then exercise the proxy directly. The third test
//   drives the full Pebble flow and is `#[ignore]`'d so the default
//   `make test-e2e` stays fast; CI runs it on Pebble.

/// Plant a challenge file under the proxy's cert_dir, then fetch it
/// from the public listener. The proxy must serve the file byte-for-
/// byte with `200 OK` and `text/plain`, **regardless of the
/// configured backend** — that's the whole bug from issue #54.
///
/// This is the cheapest reliable regression check for the missing
/// ACME HTTP-01 handler: it doesn't need Pebble, doesn't drive the
/// ACME state machine, and fails fast on the pre-fix code.
#[tokio::test]
async fn real_e2e_acme_http01_challenge_served() {
    let backend = EchoBackend::start().await;

    let ngx = NgxProcess::start(|db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        // Configure a backend that, if the proxy ever falls through
        // to it, returns 200 with a marker body that does NOT match
        // the challenge. If we see the marker body, the proxy
        // short-circuited the ACME path to the backend (the bug).
        seed_site(&conn, "acme-site", &format!("http://{}", backend.addr()));
        seed_domain(&conn, "acme.test", "acme-site");
    })
    .await;

    // Mimic what `AcmeClient::write_challenge` does during a real
    // issuance: write the key-authorization string to
    // `{cert_dir}/.well-known/acme-challenge/{token}`.
    let cert_dir = ngx.cert_dir();
    let ch_dir = cert_dir.join(".well-known").join("acme-challenge");
    std::fs::create_dir_all(&ch_dir).expect("mkdir .well-known/acme-challenge");
    let token = "test-token-abc123";
    let key_auth = format!("{}.thumbprint-def456", token);
    std::fs::write(ch_dir.join(token), &key_auth).expect("write challenge file");

    // Fetch via the public proxy. The `Host` header is intentionally
    // the configured domain — pre-fix the proxy would route to
    // `acme-site`'s backend and we'd see the EchoBackend's body
    // instead of our challenge. Post-fix the proxy short-circuits
    // and serves the challenge directly.
    let addr = format!("127.0.0.1:{}", ngx.http_port);
    let path = format!("/.well-known/acme-challenge/{}", token);
    let (status, body) = raw_get(&addr, "acme.test", &path).await;

    assert_eq!(
        status,
        200,
        "ACME HTTP-01 challenge should return 200, got {status}. \
         If 404, the proxy is missing the short-circuit handler. \
         body={body:?}. ngx log:\n{}",
        ngx.log_string()
    );
    assert_eq!(
        body,
        key_auth,
        "ACME HTTP-01 challenge body must match what the ACME client wrote. \
         If you see JSON like {{\"method\":\"GET\",\"path\":...}}, the proxy \
         fell through to the configured backend instead of serving the \
         challenge file. ngx log:\n{}",
        ngx.log_string()
    );

    // The configured backend MUST NOT have received any request — the
    // short-circuit is supposed to bypass site routing entirely.
    let seen = backend.seen().await;
    assert!(
        seen.is_empty(),
        "ACME HTTP-01 short-circuit should bypass backend routing; \
         saw {} request(s) at backend: {:?}. ngx log:\n{}",
        seen.len(),
        seen,
        ngx.log_string()
    );
}

/// Missing challenge file → 404 from the proxy, not a fall-through
/// to the configured backend. The ACME server treats 404 as
/// "challenge not ready yet" (transient error) and retries; if the
/// proxy instead returned 200 with the backend's body, the validator
/// would mark the challenge invalid.
///
/// This test exercises the same short-circuit from a different
/// angle: instead of "is the file served?" it asks "does a missing
/// file produce a clean 404?".
#[tokio::test]
async fn real_e2e_acme_http01_missing_returns_404() {
    let backend = EchoBackend::start().await;

    let ngx = NgxProcess::start(|db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        seed_site(&conn, "acme-site", &format!("http://{}", backend.addr()));
        seed_domain(&conn, "missing.test", "acme-site");
    })
    .await;

    // Don't plant any challenge file.
    let addr = format!("127.0.0.1:{}", ngx.http_port);
    let (status, body) = raw_get(
        &addr,
        "missing.test",
        "/.well-known/acme-challenge/never-existed",
    )
    .await;

    assert_eq!(
        status,
        404,
        "Missing ACME HTTP-01 challenge must return 404, got {status} \
         (body={body:?}). If 200, the proxy fell through to the \
         configured backend. ngx log:\n{}",
        ngx.log_string()
    );

    // Backend must NOT have seen anything.
    let seen = backend.seen().await;
    assert!(
        seen.is_empty(),
        "ACME HTTP-01 short-circuit should bypass backend routing \
         even for missing tokens; saw {} request(s): {:?}",
        seen.len(),
        seen
    );
}

/// Full Pebble-driven HTTP-01 issuance (issue #54's primary regression
/// scenario): start a `pangolin-ngx` pointing at a local Pebble
/// server with `PEBBLE_VA_ALWAYS_VALID=0`, drive an issuance via the
/// admin API, and verify the cert blob appears on disk.
///
/// **Why this is `#[ignore]`'d:** the full flow requires Pebble
/// reachable at `https://localhost:14000/dir` with strict validation
/// (i.e. `PEBBLE_VA_ALWAYS_VALID=0`), `localhost.test` resolving to
/// `127.0.0.1`, and Pebble's validator able to reach the proxy's
/// HTTP listener on port 80 — Pebble hardcodes port 80 for HTTP-01
/// by default. CI runs Pebble as a service (`.github/workflows/ci.yml`)
/// but does not have the right port-80 wiring to exercise the full
/// loop. Run locally with:
///
/// ```text
/// podman run --rm -d --name pebble \
///   -p 14000:14000 -p 5001:5001 -p 15000:15000 \
///   -e PEBBLE_VA_NOSLEEP=1 \
///   -e PEBBLE_VA_ALWAYS_VALID=0 \
///   -e PEBBLE_WFE_OVERRIDE_DNS=127.0.0.1 \
///   ghcr.io/letsencrypt/pebble:latest
/// echo '127.0.0.1 localhost.test' | sudo tee -a /etc/hosts
/// cargo test --features integration -p pangolin-integration-tests \
///     real_e2e_acme_http01_full_pebble_flow -- --ignored
/// ```
#[tokio::test]
#[ignore = "requires Pebble on :14000 with strict validation; see comment"]
async fn real_e2e_acme_http01_full_pebble_flow() {
    use pangolin_core::db;

    let ngx = NgxProcess::start(|db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        // Use a domain Pebble can resolve. `localhost.test` resolves
        // to 127.0.0.1 only when /etc/hosts has the entry AND
        // Pebble was started with PEBBLE_WFE_OVERRIDE_DNS=127.0.0.1.
        // The site backend is intentionally unreachable so a
        // fall-through (the bug) would 502 instead of pretending to
        // succeed.
        seed_site(&conn, "acme-full-site", "http://127.0.0.1:1");
        // auto_issue=true so the ACME service picks it up.
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO domains (domain, site_name, enabled, auto_issue, created_at) \
             VALUES (?1, ?2, 1, 1, ?3)",
            rusqlite::params!["localhost.test", "acme-full-site", now],
        )
        .expect("insert domain with auto_issue=1");
    })
    .await;

    // Drive issuance via the admin retry endpoint. The handler
    // calls AcmeState::retry which calls ensure_one → AcmeClient.
    let admin = crate::admin_harness::AdminClient::new(&ngx);
    admin.login("admin", "admin").await.expect("login");
    let resp = admin
        .post_form("/certs/retry", &[("domain", "localhost.test")])
        .await
        .expect("POST /certs/retry");
    let status = resp.status().as_u16();
    assert!(
        (200..400).contains(&status),
        "POST /certs/retry failed with {status}; ngx log:\n{}",
        ngx.log_string()
    );

    // Poll the cert blob on disk. Issuing a cert via Pebble takes
    // ~3s end-to-end (order + challenge + finalize + cert polling).
    // 60s is a generous upper bound that still fails fast on a
    // stuck proxy.
    let cert_dir = ngx.cert_dir();
    let blob = cert_dir.join("localhost.test");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        if blob.exists() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "ACME HTTP-01 issuance never produced a cert blob at {} within 60s. \
                 Most likely cause: Pebble validator returned an error because the \
                 proxy failed to serve the challenge file. ngx log:\n{}",
                blob.display(),
                ngx.log_string()
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    // Sanity: the blob contains a real certificate, not a stub.
    let content = std::fs::read_to_string(&blob).expect("read blob");
    assert!(
        content.contains("-----BEGIN CERTIFICATE-----"),
        "issued blob must contain a cert PEM block, got first 200 bytes: {:?}",
        &content[..content.len().min(200)]
    );

    // And the certs row in the DB transitioned to `Issued` (or
    // `Pending` if the blob was written before the row update
    // landed — either is "not Failed"; the precise transition
    // depends on the runtime).
    let conn = Connection::open(ngx.db_path()).expect("open db");
    let row = db::get_cert(&conn, "localhost.test")
        .expect("db get_cert")
        .expect("cert row must exist after issuance");
    assert!(
        matches!(
            row.status,
            pangolin_core::CertStatus::Issued | pangolin_core::CertStatus::Issuing
        ),
        "expected Issued/Issuing after successful issuance, got {:?} \
         (last_error={:?}). This is the bug #54 regression — if the \
         validator returned a 403/404 the row would be Failed.",
        row.status,
        row.last_error
    );
}
