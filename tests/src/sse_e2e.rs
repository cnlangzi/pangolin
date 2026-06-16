//! Server-Sent Events (SSE) and streaming-response e2e tests
//! through the tunnel.
//!
//! Background: the standard HTTP path in
//! `crates/tun/src/client.rs::handle_http_request` buffers the
//! entire response body in memory via
//! `encode_http_response(HttpResponse { body: Vec<u8>, ... })`
//! and reads `Content-Length` / `Transfer-Encoding: chunked` to
//! completion before the ngx side can write a single byte to
//! the client. For an SSE stream (`Content-Type:
//! text/event-stream`, infinite chunked response), the body
//! never ends, so the buffering path deadlocks.
//!
//! Fix: ngx detects SSE (via `is_streaming_request` in
//! `pangolin-core::proxy`) and sets `is_streaming: true` on the
//! `TunnelHttpFrame`. The tun side dispatches the request to
//! `handle_streaming_response`, which connects to the backend
//! over plain TCP, sends the request bytes, and uses
//! `tokio::io::copy_bidirectional` to relay bytes as they
//! arrive — the same pattern as `pump_ws_relay` for WebSocket.
//!
//! These tests exercise that path end-to-end: real `pangolin-ngx`
//! + real `pangolin-tun` binaries + a mock SSE backend.

use std::time::Duration;

use chrono::Utc;
use rusqlite::Connection;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::timeout;

use crate::harness::{NgxProcess, TunProcess, init_pangolin_db};

// ---------------------------------------------------------------------------
// DB seed helpers (copied from real_e2e to keep sse_e2e self-contained;
// cross-module access would require exposing them as `pub`).
// ---------------------------------------------------------------------------

fn seed_site(conn: &Connection, name: &str, backend: &str) {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO sites (name, backend, enabled, host_mode, host_custom, created_at, updated_at) \
         VALUES (?1, ?2, 1, 'passthrough', NULL, ?3, ?3)",
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
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"test-token");
    let hash = format!("{:x}", h.finalize());
    conn.execute(
        "INSERT INTO tun (name, token, token_hash, enabled, online, registered_at, last_seen_at, expires_at)
         VALUES (?1, 'test-token', ?2, ?3, 0, ?4, ?4, NULL)",
        rusqlite::params![name, hash, enabled as i32, now],
    )
    .expect("insert tun");
}

// ---------------------------------------------------------------------------
// Mock SSE backend — sends a finite (but multi-event) SSE response.
// ---------------------------------------------------------------------------

/// Mock backend that replies with `text/event-stream` and emits
/// `N` events before closing the connection. Records that it
/// saw the request so the test can assert the request actually
/// reached the backend (catches regressions where the request
/// is dropped on the wire).
struct SseBackend {
    addr: String,
    seen: std::sync::Arc<tokio::sync::Mutex<bool>>,
    handle: tokio::task::JoinHandle<()>,
}

impl SseBackend {
    /// Start a backend that emits `n_events` SSE messages,
    /// spaced `event_spacing` apart, then closes the response
    /// cleanly (the last chunk is `0\r\n\r\n` for chunked).
    async fn start(n_events: usize, event_spacing: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let seen = std::sync::Arc::new(tokio::sync::Mutex::new(false));
        let seen_for_task = seen.clone();

        let handle = tokio::spawn(async move {
            let (mut stream, _) = match listener.accept().await {
                Ok(s) => s,
                Err(_) => return,
            };
            // Mark that the backend received the request.
            *seen_for_task.lock().await = true;

            // Send SSE response headers (chunked transfer-encoding
            // because we don't know the body length up front — this
            // is the canonical SSE wire format).
            let headers = "HTTP/1.1 200 OK\r\n\
                           Content-Type: text/event-stream\r\n\
                           Cache-Control: no-cache\r\n\
                           Connection: close\r\n\
                           Transfer-Encoding: chunked\r\n\
                           \r\n";
            if stream.write_all(headers.as_bytes()).await.is_err() {
                return;
            }
            if stream.flush().await.is_err() {
                return;
            }

            // Emit N events, each as its own chunk so we can verify
            // that the **bytes arrive incrementally** through the
            // tunnel (the buffering path would never reach the client
            // because it would wait for the body to complete).
            for i in 0..n_events {
                let payload = format!("data: event {i}\n\n");
                let chunk = format!("{:x}\r\n{}\r\n", payload.len(), payload);
                if stream.write_all(chunk.as_bytes()).await.is_err() {
                    return;
                }
                if stream.flush().await.is_err() {
                    return;
                }
                tokio::time::sleep(event_spacing).await;
            }
            // Terminate the chunked response cleanly.
            let _ = stream.write_all(b"0\r\n\r\n").await;
            let _ = stream.flush().await;
        });

        Self { addr, seen, handle }
    }

    fn addr(&self) -> &str {
        &self.addr
    }

    async fn seen_request(&self) -> bool {
        *self.seen.lock().await
    }
}

impl Drop for SseBackend {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

// ---------------------------------------------------------------------------
// Raw HTTP/1.1 client that **does not** close the response on EOF —
// reads incrementally and returns the bytes received within `within`.
// ---------------------------------------------------------------------------

/// Read raw bytes from `addr` after sending a request with
/// `Accept: text/event-stream`. Returns the bytes received
/// within `within` (must be > total time the backend takes to
/// emit all events; for a real infinite stream we'd never
/// reach EOF, but the mock backend closes the chunked
/// response cleanly).
async fn raw_sse_request(
    addr: &str,
    host: &str,
    path: &str,
    within: Duration,
) -> (u16, String, String) {
    let mut stream = timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(addr))
        .await
        .expect("connect (5s)")
        .expect("connect");

    let req = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Connection: close\r\n\
         Accept: text/event-stream\r\n\
         User-Agent: pangolin-e2e-sse\r\n\
         \r\n"
    );
    stream.write_all(req.as_bytes()).await.expect("write req");
    stream.flush().await.expect("flush req");

    // Read header section (until \r\n\r\n).
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1];
    loop {
        match timeout(within, stream.read(&mut tmp)).await {
            Ok(Ok(0)) => break, // EOF
            Ok(Ok(_)) => {
                buf.push(tmp[0]);
                if buf.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            Ok(Err(e)) => panic!("read error: {e}"),
            Err(_) => panic!("read header timeout (>{within:?})"),
        }
    }
    let header_bytes = buf.clone();
    let header_str = String::from_utf8_lossy(&header_bytes).into_owned();

    // Parse status line.
    let status_line = header_str.lines().next().unwrap_or("").to_string();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    // Read body bytes until EOF or timeout. For SSE we expect
    // the backend to close after the last event, so EOF is
    // the natural terminator.
    let mut body = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        match timeout(within, stream.read(&mut tmp)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => body.extend_from_slice(&tmp[..n]),
            Ok(Err(e)) => panic!("body read error: {e}"),
            Err(_) => break, // timeout: streaming ended or hung
        }
    }
    let body_str = String::from_utf8_lossy(&body).into_owned();

    (status, header_str, body_str)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// **SSE through the tunnel path.**
///
/// Pre-fix behavior: the standard HTTP path would buffer the
/// entire chunked body in `HttpResponse.body: Vec<u8>` and
/// never return — the client would hang for 5 s and the test
/// would fail with a read timeout. The mock backend emits
/// events on a 50 ms cadence, so a working streaming path
/// must see all events arrive well within the 2 s test
/// budget.
///
/// Post-fix behavior: `is_streaming_request` matches the
/// `Accept: text/event-stream` header, ngx sets
/// `is_streaming = true` on the frame, the tun dispatches
/// to `handle_streaming_response`, and bytes are relayed
/// through `copy_bidirectional` as they arrive.
#[tokio::test]
async fn real_e2e_tunnel_sse_streams_through() {
    let backend = SseBackend::start(3, Duration::from_millis(50)).await;
    let backend_addr = backend.addr().to_string();

    let ngx = NgxProcess::start(move |db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        seed_tun(&conn, "sse", true);
        seed_site(&conn, "sse-site", &format!("sse:http://{backend_addr}"));
        seed_domain(&conn, "sse.test", "sse-site");
    })
    .await;

    let _tun = TunProcess::start(&ngx, "sse", "test-token").await;

    let addr = format!("127.0.0.1:{}", ngx.http_port);

    // The whole interaction must complete in well under the
    // pre-fix deadlock time (5 s read timeout in the raw
    // helper). 2 s is generous: 3 events × 50 ms + 1 s
    // safety margin.
    let (status, headers, body) =
        raw_sse_request(&addr, "sse.test", "/events", Duration::from_secs(2)).await;

    assert_eq!(
        status,
        200,
        "SSE request must return 200 through tunnel, got {status}. \
         headers={headers:?}\nbody={body:?}\nngx log:\n{}",
        ngx.log_string()
    );
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("content-type: text/event-stream"),
        "Content-Type must be text/event-stream, got headers: {headers:?}"
    );

    // The body must contain all three events in order.
    // Pre-fix: empty body (buffered, never returned). Post-fix:
    // all three events streamed through.
    for i in 0..3 {
        let expected = format!("data: event {i}");
        assert!(
            body.contains(&expected),
            "SSE body missing event {i}, got: {body:?}\nngx log:\n{}",
            ngx.log_string()
        );
    }

    // Sanity: the backend actually received the request. A
    // pre-fix deadlock on the **request** side (broken
    // `is_streaming` flag, or routing the request through the
    // standard HTTP path) would never reach the backend.
    assert!(
        backend.seen_request().await,
        "SSE backend never received the request — the request \
         was either dropped or routed through the wrong path. \
         ngx log:\n{}",
        ngx.log_string()
    );
}

/// **SSE through a tunnel where the backend URL contains a hostname**
/// (not a bare IP address).
///
/// ## Regression test for two related DNS-resolution bugs
///
/// `handle_streaming_response` (and its sibling `handle_ws_upgrade`)
/// in `crates/tun/src/client.rs` need to dial the backend by
/// authority string. The historical bugs and the corresponding
/// fixes are:
///
/// 1. **`SocketAddr::parse()` (syntactic-only)** — the original
///    code did `authority.parse::<SocketAddr>()`, which only
///    accepts numeric `IP:PORT` strings. A hostname like
///    `localhost:8888` or `xiajie.internal:8080` failed
///    immediately, the tun sent a synth-502 into the yamux
///    stream, and ngx surfaced a 502 Bad Gateway to the client.
///    Fix: replace with `tokio::net::lookup_host(authority)`,
///    which performs a real DNS lookup. The same fix is now in
///    `handle_ws_upgrade`.
///
/// 2. **First-result-only resolver (OS-dependent ordering)** —
///    even after switching to `lookup_host`, the code took only
///    the *first* address returned. `lookup_host` ordering is
///    OS-dependent: macOS returns v4 first for `localhost`, but
///    Linux `/etc/hosts` typically lists `::1 localhost` *before*
///    `127.0.0.1 localhost`, so on Linux the first result is
///    v6. A backend that only listens on v4 (the common case
///    — our `SseBackend` binds to `127.0.0.1:0`) would refuse
///    the v6 connect, the tun returned `Err` and dropped the
///    yamux stream, and ngx saw a 502 with "connection reset"
///    in the log. The CI Linux runner ("Pebble") caught this
///    flakey behavior; macOS local runs were unaffected. Fix:
///    iterate the full set of addresses returned by
///    `lookup_host` and connect to the first one that succeeds.
///
/// ## What this test exercises
///
/// Seeds the site backend as `sse-hostname:http://localhost:<port>`
/// (hostname, not `127.0.0.1:<port>`). On a post-fix binary
/// the resolver finds `127.0.0.1` and the SSE stream flows
/// through. The test passes on every platform because
/// `handle_streaming_response` now falls through to whichever
/// address family is reachable.
#[tokio::test]
async fn real_e2e_tunnel_sse_hostname_backend() {
    // Start a mock SSE backend on a random port.
    let backend = SseBackend::start(3, Duration::from_millis(50)).await;
    let backend_port = backend
        .addr()
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .expect("port from addr");

    // Seed the site using "localhost" (hostname) instead of "127.0.0.1".
    // This is the key difference that triggered the SocketAddr::parse bug.
    let backend_url = format!("localhost:{}", backend_port);

    let ngx = NgxProcess::start(move |db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        seed_tun(&conn, "sse-hostname", true);
        seed_site(
            &conn,
            "sse-hostname-site",
            &format!("sse-hostname:http://{backend_url}"),
        );
        seed_domain(&conn, "sse-hostname.test", "sse-hostname-site");
    })
    .await;

    let _tun = TunProcess::start(&ngx, "sse-hostname", "test-token").await;

    let addr = format!("127.0.0.1:{}", ngx.http_port);

    // Must complete well within pre-fix deadlock time.
    let (status, headers, body) = raw_sse_request(
        &addr,
        "sse-hostname.test",
        "/events",
        Duration::from_secs(2),
    )
    .await;

    assert_eq!(
        status,
        200,
        "SSE with hostname backend must return 200 (pre-fix would 502). \
         headers={headers:?}\nbody={body:?}\nngx log:\n{}",
        ngx.log_string()
    );
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("content-type: text/event-stream"),
        "Content-Type must be text/event-stream, got headers: {headers:?}"
    );
    for i in 0..3 {
        let expected = format!("data: event {i}");
        assert!(
            body.contains(&expected),
            "SSE body missing event {i}, got: {body:?}\nngx log:\n{}",
            ngx.log_string()
        );
    }
    assert!(
        backend.seen_request().await,
        "SSE backend never received a request through the hostname backend. \
         ngx log:\n{}",
        ngx.log_string()
    );
}

/// **SSE incremental delivery — events arrive spaced over time, not buffered.**
///
/// This is the *defining* property of the streaming path per
/// `docs/design/tunnel.md` §"SSE / streaming-response support":
/// "Bytes arrive at the client as they leave the backend, with no
/// in-process buffering." If the buffering path silently regressed
/// (e.g., a future refactor accidentally re-introduced the
/// `HttpResponse { body: Vec<u8> }` shape for SSE), the
/// `streams_through` test would still pass — events would arrive
/// eventually, all at once, when the backend closed. This test
/// catches that regression by asserting that the events arrive
/// *spread over time*, with the first event arriving well before
/// the last.
///
/// We give the mock backend a slow cadence (200 ms) and a
/// generous overall budget (5 s). On the streaming path, the
/// first event should reach the client in well under 1 s. On
/// any path that buffers the body until EOF, the first event
/// would not arrive until ~600 ms (after the third event has
/// been written), and this assertion would still pass — but
/// `streams_through` would also fail because the response
/// wouldn't complete in the 2 s budget. The two tests together
/// pin both liveness and incremental delivery.
#[tokio::test]
async fn real_e2e_tunnel_sse_incremental_delivery() {
    use std::time::Instant;

    let backend = SseBackend::start(3, Duration::from_millis(200)).await;
    let backend_addr = backend.addr().to_string();

    let ngx = NgxProcess::start(move |db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        seed_tun(&conn, "sse-incr", true);
        seed_site(
            &conn,
            "sse-incr-site",
            &format!("sse-incr:http://{backend_addr}"),
        );
        seed_domain(&conn, "sse-incr.test", "sse-incr-site");
    })
    .await;

    let _tun = TunProcess::start(&ngx, "sse-incr", "test-token").await;
    let addr = format!("127.0.0.1:{}", ngx.http_port);

    // Read raw bytes and timestamp each occurrence of a `data:`
    // line. With a 200 ms cadence and 3 events, the buffered
    // path would deliver all three in one flush after ~600 ms;
    // the streaming path should deliver them spread over
    // ~400 ms / 200 ms gaps. We assert that event 0 arrives at
    // most 350 ms after the request was sent (well before the
    // 600 ms total backend time).
    let mut stream = timeout(
        Duration::from_secs(5),
        tokio::net::TcpStream::connect(&addr),
    )
    .await
    .expect("connect (5s)")
    .expect("connect");

    let req = "GET /events HTTP/1.1\r\n\
               Host: sse-incr.test\r\n\
               Connection: close\r\n\
               Accept: text/event-stream\r\n\
               \r\n";
    let sent_at = Instant::now();
    stream.write_all(req.as_bytes()).await.expect("write req");
    stream.flush().await.expect("flush req");

    // Read header (up to CRLFCRLF).
    let mut hdr_buf = Vec::new();
    let mut tmp = [0u8; 1];
    loop {
        match timeout(Duration::from_secs(2), stream.read(&mut tmp)).await {
            Ok(Ok(0)) => panic!("EOF before response head"),
            Ok(Ok(_)) => {
                hdr_buf.push(tmp[0]);
                if hdr_buf.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            Ok(Err(e)) => panic!("read error: {e}"),
            Err(_) => panic!("read header timeout (2s)"),
        }
    }
    let hdr_str = String::from_utf8_lossy(&hdr_buf).into_owned();
    assert_eq!(
        hdr_str.lines().next().unwrap_or(""),
        "HTTP/1.1 200 OK",
        "first line: {hdr_str:?}"
    );
    assert!(
        hdr_str
            .to_ascii_lowercase()
            .contains("content-type: text/event-stream"),
        "Content-Type: {hdr_str:?}"
    );

    // Read body lines, recording the time each `data: event N`
    // line arrives. The buffered path would deliver all three
    // within one or two reads after the backend closes; the
    // streaming path delivers each as it arrives.
    let mut arrival = [None; 3];
    let mut body_buf: Vec<u8> = Vec::new();
    let mut read_tmp = [0u8; 1024];
    let total_budget = Duration::from_secs(2);
    loop {
        let n = match timeout(total_budget, stream.read(&mut read_tmp)).await {
            Ok(Ok(0)) => break, // backend closed
            Ok(Ok(n)) => n,
            Ok(Err(e)) => panic!("body read error: {e}"),
            Err(_) => break, // timeout: streaming ended or hung
        };
        body_buf.extend_from_slice(&read_tmp[..n]);
        for i in 0..3 {
            if arrival[i].is_none() {
                let needle = format!("data: event {i}").into_bytes();
                if body_buf.windows(needle.len()).any(|w| w == needle) {
                    arrival[i] = Some(sent_at.elapsed());
                }
            }
        }
        if arrival.iter().all(Option::is_some) && body_buf.windows(5).any(|w| w == b"0\r\n\r\n") {
            break;
        }
    }

    let t0 = arrival[0].expect("event 0 never arrived — backend did not stream");
    let t1 = arrival[1].expect("event 1 never arrived — backend did not stream");
    let t2 = arrival[2].expect("event 2 never arrived — backend did not stream");
    eprintln!(
        "SSE incremental arrival: event0={:?} event1={:?} event2={:?}",
        t0, t1, t2
    );

    // Event 0 must arrive before event 2 minus 100 ms (i.e., the
    // first event lands well before the last). A buffered path
    // would land all three at t ≈ 600 ms in one flush; the
    // assertion that event 0 arrives before t2 - 100 ms would
    // fail because event 0's t would equal event 2's t.
    assert!(
        t0 + Duration::from_millis(100) < t2,
        "events not delivered incrementally: t0={:?} t2={:?} \
         (likely the response was buffered until EOF)",
        t0,
        t2
    );
    // Event 0 must also arrive before the *full* backend time
    // (≈ 600 ms for 3 events × 200 ms cadence). A buffering
    // regression would only release event 0 at t ≈ 600 ms.
    assert!(
        t0 < Duration::from_millis(500),
        "event 0 arrived too late ({:?}); expected < 500 ms — \
         the first chunk was probably buffered until the backend \
         closed",
        t0
    );
    // Sanity: events 0/1/2 must arrive in order.
    assert!(
        t0 < t1 && t1 < t2,
        "events out of order: {t0:?} {t1:?} {t2:?}"
    );
}

/// **SSE detection with multi-value Accept header — what real
/// browser EventSource polyfills send.**
///
/// Browsers and many SSE clients send `Accept: */*` first and
/// `Accept: text/event-stream` as a second header (this is
/// permitted by RFC 7230 §3.2.2 — same name, multiple values).
/// The ngx-side detection must iterate all values; if it
/// only looks at the first Accept via `.get()` (which is the
/// pre-fix bug the code-review pass caught), this request would
/// fall through to the buffering path and the test would time
/// out.
#[tokio::test]
async fn real_e2e_tunnel_sse_multi_value_accept_header() {
    let backend = SseBackend::start(2, Duration::from_millis(50)).await;
    let backend_addr = backend.addr().to_string();

    let ngx = NgxProcess::start(move |db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        seed_tun(&conn, "sse-multi", true);
        seed_site(
            &conn,
            "sse-multi-site",
            &format!("sse-multi:http://{backend_addr}"),
        );
        seed_domain(&conn, "sse-multi.test", "sse-multi-site");
    })
    .await;

    let _tun = TunProcess::start(&ngx, "sse-multi", "test-token").await;
    let addr = format!("127.0.0.1:{}", ngx.http_port);

    // Send TWO separate Accept headers (per RFC 7230), the
    // first not matching the SSE pattern. The detection must
    // scan both values, not just .get("Accept").
    let mut stream = timeout(
        Duration::from_secs(5),
        tokio::net::TcpStream::connect(&addr),
    )
    .await
    .expect("connect (5s)")
    .expect("connect");

    let req = "GET /events HTTP/1.1\r\n\
               Host: sse-multi.test\r\n\
               Connection: close\r\n\
               Accept: */*\r\n\
               Accept: text/event-stream\r\n\
               \r\n";
    stream.write_all(req.as_bytes()).await.expect("write req");
    stream.flush().await.expect("flush req");

    // Read head.
    let mut hdr_buf = Vec::new();
    let mut tmp = [0u8; 1];
    loop {
        match timeout(Duration::from_secs(2), stream.read(&mut tmp)).await {
            Ok(Ok(0)) => panic!("EOF before response head"),
            Ok(Ok(_)) => {
                hdr_buf.push(tmp[0]);
                if hdr_buf.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            Ok(Err(e)) => panic!("read error: {e}"),
            Err(_) => panic!("read header timeout (2s) — SSE detection missed multi-Accept"),
        }
    }
    let hdr_str = String::from_utf8_lossy(&hdr_buf).into_owned();
    assert_eq!(
        hdr_str.lines().next().unwrap_or(""),
        "HTTP/1.1 200 OK",
        "multi-Accept did not route to the streaming path: {hdr_str:?}"
    );
    assert!(
        hdr_str
            .to_ascii_lowercase()
            .contains("content-type: text/event-stream"),
        "Content-Type: {hdr_str:?}"
    );

    // Read body and confirm both events arrived.
    let mut body = Vec::new();
    let mut read_tmp = [0u8; 1024];
    loop {
        match timeout(Duration::from_secs(2), stream.read(&mut read_tmp)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => body.extend_from_slice(&read_tmp[..n]),
            Ok(Err(e)) => panic!("body read error: {e}"),
            Err(_) => break,
        }
    }
    let body_str = String::from_utf8_lossy(&body).into_owned();
    for i in 0..2 {
        assert!(
            body_str.contains(&format!("data: event {i}")),
            "missing event {i}: body={body_str:?}\nngx log:\n{}",
            ngx.log_string()
        );
    }
}

/// **SSE keep-alive comments — a real-world SSE feature the
/// pre-fix buffering path would also drop.**
///
/// SSE lets the server send `:keepalive\n\n` (a comment line)
/// to keep proxies and clients from idle-timing out the
/// connection. The mock backend sends one comment between
/// each data event, matching what observability tools and
/// chat-streaming services do in production. The test verifies
/// that the relay preserves these comments byte-for-byte (a
/// hand-rolled HTTP head parser that mishandled `:keepalive`
/// would either strip the leading colon or break the chunk
/// boundary).
#[tokio::test]
async fn real_e2e_tunnel_sse_preserves_keepalive_comments() {
    use std::time::Duration as StdDuration;

    // A backend that emits 2 data events with a `:keepalive`
    // comment line before the first data event.
    async fn start_keepalive_backend() -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let handle = tokio::spawn(async move {
            let (mut stream, _) = match listener.accept().await {
                Ok(s) => s,
                Err(_) => return,
            };
            let headers = "HTTP/1.1 200 OK\r\n\
                           Content-Type: text/event-stream\r\n\
                           Cache-Control: no-cache\r\n\
                           Connection: close\r\n\
                           Transfer-Encoding: chunked\r\n\
                           \r\n";
            if stream.write_all(headers.as_bytes()).await.is_err() {
                return;
            }
            let _ = stream.flush().await;
            // Comment line, then first data event.
            let comment = ": keepalive\r\n";
            let data0 = "data: hello\n\n";
            let chunk0 = format!(
                "{:x}\r\n{}{}\r\n{:x}\r\n{}\r\n",
                comment.len(),
                comment,
                data0,
                data0.len(),
                data0,
            );
            let _ = stream.write_all(chunk0.as_bytes()).await;
            let _ = stream.flush().await;
            tokio::time::sleep(StdDuration::from_millis(50)).await;
            // Second data event.
            let data1 = "data: world\n\n";
            let chunk1 = format!("{:x}\r\n{}\r\n", data1.len(), data1);
            let _ = stream.write_all(chunk1.as_bytes()).await;
            let _ = stream.flush().await;
            let _ = stream.write_all(b"0\r\n\r\n").await;
            let _ = stream.flush().await;
        });
        (addr, handle)
    }

    let (backend_addr, handle) = start_keepalive_backend().await;
    let ngx = NgxProcess::start(move |db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        seed_tun(&conn, "sse-keepalive", true);
        seed_site(
            &conn,
            "sse-keepalive-site",
            &format!("sse-keepalive:http://{backend_addr}"),
        );
        seed_domain(&conn, "sse-keepalive.test", "sse-keepalive-site");
    })
    .await;

    let _tun = TunProcess::start(&ngx, "sse-keepalive", "test-token").await;
    let addr = format!("127.0.0.1:{}", ngx.http_port);

    let (status, headers, body) = raw_sse_request(
        &addr,
        "sse-keepalive.test",
        "/events",
        Duration::from_secs(2),
    )
    .await;

    assert_eq!(
        status, 200,
        "got {status}, headers={headers:?}, body={body:?}"
    );
    assert!(
        body.contains(": keepalive"),
        "lost :keepalive comment: {body:?}"
    );
    assert!(
        body.contains("data: hello"),
        "lost first data event: {body:?}"
    );
    assert!(
        body.contains("data: world"),
        "lost second data event: {body:?}"
    );

    handle.abort();
}

/// **`is_streaming_request` detection unit test (pangolin-core).**
///
/// Verifies the heuristics directly without spawning binaries.
#[test]
fn is_streaming_request_detects_text_event_stream() {
    use pangolin_core::is_streaming_request;
    use pangolin_core::tunnel::HttpRequest;

    let mk = |accept: &str| HttpRequest {
        method: "GET".into(),
        target: "/events".into(),
        version: "HTTP/1.1".into(),
        headers: vec![("Accept".into(), accept.into())],
        body: vec![],
    };

    assert!(is_streaming_request(&mk("text/event-stream")));
    assert!(is_streaming_request(&mk("*/*, text/event-stream")));
    assert!(is_streaming_request(&mk("TEXT/EVENT-STREAM")));
    assert!(!is_streaming_request(&mk("application/json")));

    let no_accept = HttpRequest {
        method: "GET".into(),
        target: "/".into(),
        version: "HTTP/1.1".into(),
        headers: vec![("Host".into(), "x.test".into())],
        body: vec![],
    };
    assert!(!is_streaming_request(&no_accept));
}
