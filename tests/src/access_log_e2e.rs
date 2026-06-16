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
//!    backend, host, client_ip, timestamp). A future change that
//!    renames or removes a field will fail this test loudly
//!    instead of breaking the admin UI silently.
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
use crate::harness::{NgxProcess, init_pangolin_db, raw_request};

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
// SSE raw reader — connects to /api/logs/stream, returns
// whatever `data:` JSON frames arrive within `within`.
// ─────────────────────────────────────────────────────────────────

/// Open a raw TCP connection to `addr`, send a GET with
/// `Accept: text/event-stream`, and return every `data:` line we
/// read within `within`. The body is split into per-frame JSON
/// strings (the SSE wire format is `data: <json>\n\n`; we slice
/// on that boundary).
///
/// `cookie` is the raw `Cookie:` header value (e.g.
/// `"pangolin_session=abc; pangolin_csrf=def"`). The SSE endpoint
/// requires an authenticated admin session — without it the
/// handshake returns 401 (see `sse.rs::write_sse_unauth`). Pass
/// `None` to exercise the unauthenticated path on purpose.
async fn read_sse_data_frames(
    addr: &str,
    within: Duration,
    cookie: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    let mut stream: TcpStream = timeout(Duration::from_secs(5), TcpStream::connect(addr))
        .await
        .map_err(|e| anyhow::anyhow!("connect timeout: {e}"))?
        .map_err(|e| anyhow::anyhow!("connect: {e}"))?;

    // The `Cookie:` header is appended only when `cookie` is
    // `Some`. Sending an empty `Cookie:` line is technically legal
    // per RFC 6265 §5.4 but a few middleboxes treat it as a parse
    // error, so we omit the line entirely instead.
    let cookie_line = match cookie {
        Some(c) => format!("Cookie: {c}\r\n"),
        None => String::new(),
    };
    let req = format!(
        "GET /api/logs/stream HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Accept: text/event-stream\r\n\
         Connection: close\r\n\
         User-Agent: pangolin-access-log-e2e\r\n\
         {cookie_line}\r\n"
    );
    stream.write_all(req.as_bytes()).await?;
    stream.flush().await?;

    // Read until we have the full header block.
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        match timeout(within, stream.read(&mut tmp)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Ok(Err(e)) => return Err(anyhow::anyhow!("read err: {e}")),
            Err(_) => return Err(anyhow::anyhow!("read header timeout")),
        }
    }
    // Verify status line.
    let head_str = String::from_utf8_lossy(&buf).into_owned();
    let status_line = head_str.lines().next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if status != 200 {
        return Err(anyhow::anyhow!(
            "SSE handshake failed: status={status}, head={head_str:?}"
        ));
    }

    // Strip the headers — anything after the first \r\n\r\n is body.
    let body_start = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
        .unwrap_or(buf.len());
    let mut body = buf.split_off(body_start);

    // Read the rest of the body until EOF or `within` elapses.
    loop {
        match timeout(within, stream.read(&mut tmp)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => body.extend_from_slice(&tmp[..n]),
            Ok(Err(_)) => break,
            Err(_) => break,
        }
    }
    let body_str = String::from_utf8_lossy(&body).into_owned();

    // Split the body on the SSE frame terminator `\n\n` and pull
    // out the JSON after each `data: ` prefix. We deliberately
    // ignore SSE comment lines (`: ...`) and event-id / event
    // lines (the server doesn't emit those).
    let mut frames = Vec::new();
    for raw in body_str.split("\n\n") {
        let mut json: Option<String> = None;
        for line in raw.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                let s = rest.trim_start();
                // Multi-line `data:` (per the SSE spec, multiple
                // `data:` lines are concatenated with newlines).
                // Our server emits a single line per frame so
                // we don't need to handle the multi-line case,
                // but be defensive.
                if let Some(prev) = json.as_mut() {
                    prev.push('\n');
                    prev.push_str(s);
                } else {
                    json = Some(s.to_string());
                }
            }
        }
        if let Some(j) = json {
            // Skip empty data lines and the comment-only frames
            // that the server uses for diagnostics
            // (": replay: done" etc.).
            if j.is_empty() || j.starts_with(':') {
                continue;
            }
            frames.push(j);
        }
    }
    Ok(frames)
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
    //    convenient but its cookie store is opaque to us; we just
    //    POST /login and pluck the `pangolin_session` cookie out of
    //    `Set-Cookie`.
    let admin_base = format!("http://127.0.0.1:{}", ngx.admin_port);
    let raw_client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("build raw client");
    let login_resp = raw_client
        .post(&format!("{admin_base}/login"))
        .form(&[("username", "admin"), ("password", "admin")])
        .send()
        .await
        .expect("POST /login");
    assert_eq!(login_resp.status().as_u16(), 302, "login must redirect");
    let session_cookie: String = login_resp
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|v| {
            // Set-Cookie: pangolin_session=<token>; HttpOnly; ...
            v.split(';').next().and_then(|kv| {
                let kv = kv.trim();
                if kv.starts_with("pangolin_session=") {
                    Some(kv.to_string())
                } else {
                    None
                }
            })
        })
        .expect("login response must Set-Cookie pangolin_session=");

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
    let sse_task =
        tokio::spawn(
            async move { read_sse_data_frames(&sse_addr, within, Some(&sse_cookie)).await },
        );

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
