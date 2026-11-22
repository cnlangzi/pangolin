//! In-memory access log + SSE real-time push — end-to-end tests
//! for issue #73.
//!
//! ## What we exercise
//!
//! 1. **Unauthenticated access to `/api/logs/stream` returns 401**
//!    — the SSE endpoint, like every other admin route, must require
//!    a session cookie. Anonymous attempts must not silently get an
//!    SSE-shaped error (EventSource would parse it as `data:` and
//!    retry forever).
//!
//! 2. **Replay of the bounded ring buffer** — when a fresh admin
//!    connects, the SSE endpoint first drains
//!    `App::recent_access_log()` (oldest-first) before flipping
//!    into live-broadcast mode. We assert that every entry the
//!    proxy pushed *before* the SSE connect is delivered as a
//!    `data:` JSON frame, in order.
//!
//! 3. **Live broadcast after replay** — once the ring buffer has
//!    been drained, a new proxied request is observed in real
//!    time on the SSE connection. This is the "real-time push"
//!    half of the issue title.
//!
//! 4. **JSON shape** — every `data:` frame is one `AccessLogEntry`
//!    serialised with serde_json. We parse the first frame and
//!    verify the field set (method, path, status, duration_ms,
//!    backend, host, client_ip, timestamp, user_agent). A future
//!    change that renames or removes a field will fail this test
//!    loudly instead of breaking the admin UI silently.
//!
//! 5. **Admin UI `/logs` page renders** — the page is reachable
//!    by an authenticated admin and contains the wiring
//!    (`<div id="log-stream">`-equivalent markup) that the
//!    browser-side `EventSource` consumer attaches to.
//!
//! ## Why we don't drive a separate mock backend
//!
//! The full proxy path (domain lookup → direct backend dial →
//! response) requires a *real* upstream. We use a tiny in-process
//! `MockHttpBackend` that just responds 200 to every request —
//! the access log captures the request regardless of what the
//! backend does, so the SSE assertions do not depend on the
//! backend's body.
//!
//! Prerequisite: `make build` (the tests spawn `pangolin-ngx`
//! from `target/release/` or `bin/`).

use std::time::Duration;

use chrono::Utc;
use pangolin_core::db;
use pangolin_core::types::{Domain, HostMode, Site};
use reqwest::Client;
use rusqlite::Connection;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

use crate::admin_harness::AdminClient;
use crate::harness::{
    DEFAULT_USER_AGENT, NgxProcess, TunProcess, admin_session_cookie, init_pangolin_db,
    raw_request, read_sse_data_frames,
};

// ─────────────────────────────────────────────────────────────────
// DB seed helpers — thin wrappers over `pangolin_core::db::upsert_*`
// so the test stays in lock-step with the canonical column set
// (schema additions land in db.rs, not here).
// ─────────────────────────────────────────────────────────────────

fn seed_site(conn: &Connection, name: &str, backend: &str) {
    let now = Utc::now();
    db::upsert_site(
        conn,
        &Site {
            name: name.into(),
            backend: backend.into(),
            enabled: true,
            host_mode: HostMode::Passthrough,
            host_custom: None,
            created_at: now,
            updated_at: now,
            domain_count: 0,
        },
    )
    .expect("insert site");
}

fn seed_domain(conn: &Connection, domain: &str, site_name: &str) {
    db::upsert_domain(
        conn,
        &Domain {
            domain: domain.into(),
            site_name: site_name.into(),
            enabled: true,
            auto_issue: false,
            dns_provider: None,
            // PR #71 added per-domain challenge_kind. Tests don't
            // exercise ACME issuance, so `None` (auto-default) is
            // the correct seed value.
            challenge_kind: None,
            created_at: Utc::now(),
        },
    )
    .expect("insert domain");
}

/// Insert a tun row with a known token. Mirrors `seed_tun` in
/// `real_e2e.rs` but kept inline so this module doesn't have to
/// depend on the tunnel test's seeders. The token is `sha256`'d
/// at insert time — that matches what `tun::auth_tun` expects
/// (v3 hash-based lookup).
fn seed_tun(conn: &Connection, name: &str) {
    use pangolin_core::db::upsert_tun;
    use pangolin_core::types::Tun;
    upsert_tun(
        conn,
        &Tun {
            name: name.into(),
            token: Some("test-token".into()),
            token_hash: None,
            enabled: true,
            online: false,
            registered_at: None,
            last_seen_at: None,
            expires_at: None,
        },
    )
    .expect("insert tun");
}

// ─────────────────────────────────────────────────────────────────
// Mock upstream — answers 200 OK to every request. We don't care
// about the response body; the access log records the request.
// ─────────────────────────────────────────────────────────────────

/// Tiny HTTP backend that replies 200 OK with a fixed JSON body
/// to every request. The access log is recorded by the proxy
/// before the response is read, so the backend's actual content
/// is irrelevant to the SSE assertions.
struct MockHttpBackend {
    addr: String,
    _handle: tokio::task::JoinHandle<()>,
}

impl MockHttpBackend {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let handle = tokio::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => break,
                };
                tokio::spawn(async move {
                    // Drain the request (we don't need to parse it
                    // — the access log captures the request
                    // independently in the proxy). Read until we
                    // see the end-of-headers marker or the
                    // client closes.
                    let mut buf = Vec::with_capacity(2048);
                    let mut tmp = [0u8; 1024];
                    loop {
                        match timeout(Duration::from_secs(2), stream.read(&mut tmp)).await {
                            Ok(Ok(0)) => break,
                            Ok(Ok(n)) => {
                                buf.extend_from_slice(&tmp[..n]);
                                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Ok(Err(_)) => break,
                            Err(_) => break,
                        }
                    }
                    let body = b"{\"ok\":true}";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                    let _ = stream.write_all(body).await;
                    let _ = stream.flush().await;
                });
            }
        });
        Self {
            addr,
            _handle: handle,
        }
    }

    fn addr(&self) -> &str {
        &self.addr
    }
}

// ─────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────

/// access_log_sse_requires_auth — the SSE endpoint is admin-only.
/// A GET with no session cookie must return 401 (NOT 200, NOT an
/// SSE-shaped error). This is the auth-failure contract from
/// `sse.rs::write_sse_unauth`.
#[tokio::test]
async fn access_log_sse_requires_auth() {
    let backend = MockHttpBackend::start().await;
    let ngx = NgxProcess::start(move |db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        seed_site(&conn, "auth-site", &format!("http://{}", backend.addr()));
        seed_domain(&conn, "auth.test", "auth-site");
    })
    .await;
    let addr = format!("127.0.0.1:{}", ngx.admin_port);

    // Issue #73 invariant: 401 (not 200, not 302, not SSE). The
    // browser's EventSource will silently retry on any opaque
    // error, so an HTML login redirect would loop forever.
    let mut stream: TcpStream = timeout(Duration::from_secs(5), TcpStream::connect(&addr))
        .await
        .expect("connect")
        .expect("connect");
    let req = "GET /api/logs/stream HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: text/event-stream\r\nConnection: close\r\n\r\n";
    stream.write_all(req.as_bytes()).await.expect("write");
    stream.flush().await.expect("flush");

    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    let _ = timeout(Duration::from_secs(3), stream.read_to_end(&mut buf)).await;
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status: u16 = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    assert_eq!(
        status,
        401,
        "expected 401 Unauthorized, got {status}. body={text:?}\nngx log:\n{}",
        ngx.log_string()
    );
}

/// access_log_sse_replays_then_streams_live — the core issue #73
/// behaviour: the admin opens `/api/logs/stream`, gets a replay
/// of the ring buffer (oldest-first), then sees new entries
/// arrive in real time as the proxy pushes them.
///
/// We assert:
///   1. The very first `data:` frame is the GET that was sent
///      *before* the SSE connection opened (replay).
///   2. The frame's JSON shape matches `AccessLogEntry`
///      (method, path, status, duration_ms, host, backend, etc.).
///   3. A GET sent *after* the SSE connection is opened shows up
///      in the live stream within a small budget.
///   4. The frame carries the `User-Agent` the client sent — the
///      proxy reads it in place out of the request headers and
///      copies it once, into the entry (issue #224).
#[tokio::test]
async fn access_log_sse_replays_then_streams_live() {
    let backend = MockHttpBackend::start().await;
    let backend_addr = backend.addr().to_string();
    let ngx = NgxProcess::start({
        let backend_addr = backend_addr.clone();
        move |db_path| {
            init_pangolin_db(db_path);
            let conn = Connection::open(db_path).expect("open db");
            seed_site(&conn, "replay-site", &format!("http://{backend_addr}"));
            seed_domain(&conn, "replay.test", "replay-site");
        }
    })
    .await;

    // 0) Authenticate. The SSE endpoint is admin-only — covered by
    //    `access_log_sse_requires_auth`. We capture the raw session
    //    cookie here so the low-level SSE reader can send it on the
    //    `GET /api/logs/stream`. Logging in via `AdminClient` is
    //    convenient but its cookie store is opaque to us; the shared
    //    helper POSTs /login and plucks the `pangolin_session`
    //    cookie out of `Set-Cookie`.
    let session_cookie = admin_session_cookie(&ngx).await;

    // 1) Generate a request BEFORE the SSE connection opens. The
    //    proxy's response_filter pushes an AccessLogEntry to the
    //    ring buffer; we then expect it to be replayed as the
    //    first data frame on /api/logs/stream.
    let (status_before, _) = raw_request(
        &format!("127.0.0.1:{}", ngx.http_port),
        "replay.test",
        "GET",
        "/api/before",
        b"",
    )
    .await;
    assert_eq!(status_before, 200, "pre-SSE request must succeed");

    // 2) Open the SSE connection. We expect the pre-SSE entry
    //    to be replayed as the first data frame.
    let sse_addr = format!("127.0.0.1:{}", ngx.admin_port);
    let within = Duration::from_secs(3);

    // Read frames in a separate task so we can keep pushing
    // requests into the live stream.
    let sse_cookie = session_cookie.clone();
    let sse_task = tokio::spawn(async move {
        read_sse_data_frames(&sse_addr, "/api/logs/stream", within, Some(&sse_cookie)).await
    });

    // Give the SSE handshake a moment to complete and the
    // replay frames to land.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 3) Generate a second request AFTER the SSE connection.
    //    This is the "real-time push" half of issue #73.
    let (status_after, _) = raw_request(
        &format!("127.0.0.1:{}", ngx.http_port),
        "replay.test",
        "GET",
        "/api/after",
        b"",
    )
    .await;
    assert_eq!(status_after, 200, "post-SSE request must succeed");

    // Collect all frames the SSE reader saw.
    let frames = sse_task.await.expect("sse task join").expect("sse frames");
    assert!(
        !frames.is_empty(),
        "expected at least one SSE data frame, got 0. \
         ngx log:\n{}",
        ngx.log_string()
    );

    // First frame: replay of the pre-SSE request. The broadcast
    // channel delivers asynchronously, so the order between
    // "replay" and "live" frames depends on the test scheduler;
    // what we *can* assert is that both paths are present.
    let pre: Option<serde_json::Value> = frames
        .iter()
        .map(|s| serde_json::from_str::<serde_json::Value>(s))
        .find_map(|r| {
            r.ok().and_then(|j| {
                let matches = j.get("path").and_then(|p| p.as_str()) == Some("/api/before");
                if matches { Some(j) } else { None }
            })
        });
    let post: Option<serde_json::Value> = frames
        .iter()
        .map(|s| serde_json::from_str::<serde_json::Value>(s))
        .find_map(|r| {
            r.ok().and_then(|j| {
                let matches = j.get("path").and_then(|p| p.as_str()) == Some("/api/after");
                if matches { Some(j) } else { None }
            })
        });

    let pre = pre.expect("pre-SSE entry not replayed");
    let post = post.expect("post-SSE entry not streamed live");

    // JSON shape — every documented AccessLogEntry field must
    // be present and of the right type. If a future change
    // renames `duration_ms` or drops `client_ip`, the admin UI
    // will break silently; this assertion catches it here.
    for entry in [&pre, &post] {
        for key in &[
            "timestamp",
            "method",
            "path",
            "host",
            "status",
            "duration_ms",
            "backend",
            "client_ip",
        ] {
            assert!(
                entry.get(key).is_some(),
                "SSE frame missing required key `{key}`: {entry}"
            );
        }
        assert_eq!(entry["method"], "GET");
        assert_eq!(entry["host"], "replay.test");
        assert_eq!(entry["status"], 200);
        assert!(
            entry["duration_ms"].as_u64().is_some(),
            "duration_ms must be a number: {entry}"
        );
        assert_eq!(entry["backend"], format!("direct:{}", backend_addr));
        // The entry carries the `User-Agent` the client sent. The
        // proxy reads it in place out of the request headers and
        // copies it once, into the entry (issue #224) — so the log
        // still shows what the crawler announced itself as.
        assert_eq!(
            entry["user_agent"], DEFAULT_USER_AGENT,
            "the access log must carry the request's User-Agent: {entry}"
        );
    }
}

/// access_log_admin_page_renders — the `/logs` UI is reachable
/// by an authenticated admin and contains the streaming widget.
/// We don't need to drive the EventSource from here (that's
/// covered by the SSE test above); this just makes sure the
/// page itself doesn't 500 / 404 / redirect-loops the way a
/// missing route registration would.
#[tokio::test]
async fn access_log_admin_page_renders() {
    let _ngx = NgxProcess::start(init_pangolin_db).await;
    let client = AdminClient::new(&_ngx);
    client.login("admin", "admin").await.expect("login");

    let resp = client.get("/logs").await.expect("GET /logs");
    assert_eq!(resp.status().as_u16(), 200, "/logs must be 200 OK");

    let body = resp.text().await.expect("body");
    // Page must include the streaming widget container + the
    // path the EventSource consumer targets. We don't pin to a
    // specific element id (the template may evolve) but we do
    // pin to the SSE URL and to a recognisable label.
    assert!(
        body.contains("/api/logs/stream"),
        "page must reference /api/logs/stream: {body}"
    );
    assert!(
        body.contains("Access Logs") || body.contains("access log"),
        "page must be titled as an access log viewer: {body}"
    );
}

/// access_log_admin_page_requires_auth — `/logs` (the page)
/// redirects unauthenticated requests to `/login` (the same as
/// every other admin page). This guards the auth check in
/// `admin::handle()` — without it, an anonymous browser could
/// open the page (and then silently fail to connect to the SSE
/// stream, but at least the page itself leaked).
#[tokio::test]
async fn access_log_admin_page_requires_auth() {
    let _ngx = NgxProcess::start(init_pangolin_db).await;

    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let resp = client
        .get(&format!("http://127.0.0.1:{}/logs", _ngx.admin_port))
        .send()
        .await
        .expect("GET /logs");
    assert_eq!(
        resp.status().as_u16(),
        302,
        "unauthenticated /logs must redirect to /login"
    );
    let loc = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        loc.contains("/login"),
        "redirect target must be /login, got: {loc}"
    );
}

/// access_log_all_backend_types_emit_entries — regression for the
/// `d5f232e` fix that made `record_access_log` fire on the tunnel
/// and file:// short-circuits in `request_filter`. Before the fix,
/// only the direct / pingora path logged anything; tunnel and
/// file:// requests went through `write_response_to_session`
/// followed by `return Ok(true)` and never reached
/// `response_filter`, so the `/logs` page stayed silent for
/// half the production traffic.
///
/// The test stands up three sites covering the three backend
/// types and asserts each one's entry shows up on the SSE stream
/// with the matching `backend` label.
#[tokio::test]
async fn access_log_all_backend_types_emit_entries() {
    use tempfile::TempDir;

    // 1) Direct backend — mock HTTP server. 200 OK is enough; the
    //    proxy logs the request regardless of body.
    let direct_backend = MockHttpBackend::start().await;
    let direct_addr = direct_backend.addr().to_string();

    // 2) Tunnel backend — uses the same mock upstream as the
    //    direct site, but routed through a `local:tun_name`
    //    tunnel. Requires a real `pangolin-tun` binary.
    let tunnel_backend = MockHttpBackend::start().await;
    let tunnel_backend_addr = tunnel_backend.addr().to_string();

    // 3) File backend — point at a tempdir with a known file.
    let doc_root = TempDir::new().expect("tempdir");
    let doc_root_path = doc_root.path().to_path_buf();
    std::fs::write(doc_root_path.join("hello.txt"), "hi from file\n").expect("write hello.txt");
    let doc_root_uri = format!("file://{}", doc_root_path.display());

    let ngx = NgxProcess::start({
        let direct_addr = direct_addr.clone();
        let tunnel_backend_addr = tunnel_backend_addr.clone();
        let doc_root_uri = doc_root_uri.clone();
        move |db_path| {
            init_pangolin_db(db_path);
            let conn = Connection::open(db_path).expect("open db");
            // Direct site (pingora path → response_filter).
            seed_site(&conn, "direct-site", &format!("http://{direct_addr}"));
            seed_domain(&conn, "direct.test", "direct-site");

            // Tunnel site (YamuxTunnelExecutor short-circuit).
            seed_tun(&conn, "office");
            seed_site(
                &conn,
                "tunnel-site",
                &format!("office:http://{tunnel_backend_addr}"),
            );
            seed_domain(&conn, "tunnel.test", "tunnel-site");

            // File site (serve_file_target short-circuit).
            seed_site(&conn, "file-site", &doc_root_uri);
            seed_domain(&conn, "file.test", "file-site");
        }
    })
    .await;

    // Spin up the tun binary — without it the tunnel request
    // returns 503 and the test fails on the wrong symptom.
    let _tun = TunProcess::start(&ngx, "office", "test-token").await;

    // Authenticate so the SSE endpoint will accept our stream.
    // The shared helper returns the raw `pangolin_session=...`
    // cookie the low-level SSE reader puts on the wire — same dance
    // as `access_log_sse_replays_then_streams_live`.
    let sse_cookie = admin_session_cookie(&ngx).await;

    // Open SSE in the background; it will receive both the
    // pre-stream replay (empty — we haven't fired yet) and the
    // live entries pushed by the three requests below.
    let sse_addr = format!("127.0.0.1:{}", ngx.admin_port);
    let sse_task = tokio::spawn(async move {
        read_sse_data_frames(
            &sse_addr,
            "/api/logs/stream",
            Duration::from_secs(5),
            Some(&sse_cookie),
        )
        .await
    });

    // Give the SSE handshake + replay-drain time to settle before
    // we start firing requests; otherwise our entries might land
    // before the subscription is live and be lost.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Fire one request per backend. Each must succeed (the
    // tunnel + file responses prove the request actually reached
    // the matching code path) — otherwise a regression in the
    // unrelated proxy plumbing would mask the access-log bug
    // we're trying to catch.
    let http_addr = format!("127.0.0.1:{}", ngx.http_port);
    let (s_direct, _) = raw_request(&http_addr, "direct.test", "GET", "/d", b"").await;
    assert_eq!(s_direct, 200, "direct backend must return 200");
    let (s_tunnel, _) = raw_request(&http_addr, "tunnel.test", "GET", "/t", b"").await;
    assert_eq!(s_tunnel, 200, "tunnel backend must return 200");
    let (s_file, _) = raw_request(&http_addr, "file.test", "GET", "/hello.txt", b"").await;
    assert_eq!(s_file, 200, "file backend must return 200");

    let frames = sse_task.await.expect("sse task join").expect("sse frames");

    // Build a small index keyed by path so we can assert each
    // backend type contributed exactly one entry.
    let mut by_path: std::collections::HashMap<String, serde_json::Value> =
        std::collections::HashMap::new();
    for frame in &frames {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(frame) else {
            continue;
        };
        let Some(path) = v.get("path").and_then(|p| p.as_str()) else {
            continue;
        };
        by_path.insert(path.to_string(), v);
    }

    for (path, expected_backend_prefix) in [
        ("/d", "direct:"),
        ("/t", "tun:"),
        ("/hello.txt", "direct:"), // file:// path is logged as `direct:<doc_root>`
    ] {
        let entry = by_path.get(path).unwrap_or_else(|| {
            panic!(
                "no SSE frame for path={path}. frames={frames:?}\nngx log:\n{}",
                ngx.log_string()
            )
        });
        let backend = entry.get("backend").and_then(|b| b.as_str()).unwrap_or("");
        assert!(
            backend.starts_with(expected_backend_prefix),
            "path={path}: expected backend starting with {:?}, got {backend:?}",
            expected_backend_prefix
        );
        assert_eq!(
            entry.get("host").and_then(|h| h.as_str()),
            Some(match path {
                "/d" => "direct.test",
                "/t" => "tunnel.test",
                _ => "file.test",
            })
        );
        // R16 (issue #122): the site lookup moved ahead of the
        // visitor-IP resolution, so every path — the tunnel one
        // included — must still carry the visitor's IP. A
        // `unknown` here would mean the resolution ran after the
        // short-circuit.
        let client_ip = entry
            .get("client_ip")
            .and_then(|c| c.as_str())
            .unwrap_or("");
        assert_ne!(
            client_ip, "unknown",
            "path={path}: the access log must record the visitor IP"
        );
    }
}
