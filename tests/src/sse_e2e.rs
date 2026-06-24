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

use std::time::{Duration, Instant};

use chrono::Utc;
use reqwest::Client;
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
// Mock SSE backend that **records** when its peer closes the
// TCP connection. Used by the disconnect-propagation e2e test
// to verify that the tun-side `handle_streaming_response`
// promptly closes the backend TCP after the ngx-side yamux
// stream drops on client disconnect. See
// `docs/design/sse-reconnect.md`.
// ---------------------------------------------------------------------------

/// Mock backend that emits events forever on a fixed cadence
/// and records the instant its peer closes the TCP connection.
///
/// Implementation: `tokio::io::split` separates the accepted
/// `TcpStream` into independent read and write halves. A
/// **watchdog task** owns the read half and blocks on a
/// one-byte read — when the peer closes (FIN) or the connection
/// is reset (RST), the read returns and the watchdog stamps
/// `peer_closed_at`. The **main task** owns the write half and
/// loops emitting events forever; it exits when its own write
/// fails (peer gone) or when the test ends and `Drop` aborts
/// both tasks. See `docs/design/sse-reconnect.md`.
struct ObservableSseBackend {
    addr: String,
    seen: std::sync::Arc<tokio::sync::Mutex<bool>>,
    peer_closed_at: std::sync::Arc<tokio::sync::Mutex<Option<Instant>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl ObservableSseBackend {
    async fn start(event_spacing: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let seen = std::sync::Arc::new(tokio::sync::Mutex::new(false));
        let peer_closed_at = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let seen_for_task = seen.clone();
        let peer_closed_for_task = peer_closed_at.clone();

        let handle = tokio::spawn(async move {
            let (stream, _) = match listener.accept().await {
                Ok(s) => s,
                Err(_) => return,
            };
            *seen_for_task.lock().await = true;

            // Split into independent read/write halves so a
            // watchdog task can block on read (peer-close probe)
            // while the main task writes events. The two halves
            // share the underlying socket via Arc<Mutex<Inner>>;
            // this is the standard tokio pattern for concurrent
            // read+write on one stream.
            let (mut reader, mut writer) = tokio::io::split(stream);

            // Send SSE response headers (chunked transfer-encoding).
            let headers = "HTTP/1.1 200 OK\r\n\
                           Content-Type: text/event-stream\r\n\
                           Cache-Control: no-cache\r\n\
                           Connection: close\r\n\
                           Transfer-Encoding: chunked\r\n\
                           \r\n";
            if writer.write_all(headers.as_bytes()).await.is_err() {
                return;
            }
            if writer.flush().await.is_err() {
                return;
            }

            // Watchdog task: blocks on a one-byte read. When the
            // peer closes (FIN/RST), the read returns Ok(0) or
            // Err and we stamp `peer_closed_at`.
            let peer_closed_watchdog = peer_closed_for_task.clone();
            let watchdog = tokio::spawn(async move {
                let mut probe = [0u8; 1];
                let _ = reader.read(&mut probe).await;
                *peer_closed_watchdog.lock().await = Some(Instant::now());
            });

            // Main task: emit events forever. Stops when its own
            // write fails (peer gone) or the test ends (Drop).
            let mut i: usize = 0;
            loop {
                let payload = format!("data: event {i}\n\n");
                let chunk = format!("{:x}\r\n{}\r\n", payload.len(), payload);
                if writer.write_all(chunk.as_bytes()).await.is_err() {
                    break;
                }
                if writer.flush().await.is_err() {
                    break;
                }
                i += 1;
                tokio::time::sleep(event_spacing).await;
            }
            watchdog.abort();
        });

        Self { addr, seen, peer_closed_at, handle }
    }

    fn addr(&self) -> &str { &self.addr }

    async fn seen_request(&self) -> bool { *self.seen.lock().await }

    async fn peer_closed_at(&self) -> Option<Instant> { *self.peer_closed_at.lock().await }
}

impl Drop for ObservableSseBackend {
    fn drop(&mut self) { self.handle.abort(); }
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
/// **Client disconnect propagates promptly to the backend.**
///
/// ## What this test exercises
///
/// The flow:
///
/// 1. Boot `pangolin-ngx` + `pangolin-tun` against an
///    [`ObservableSseBackend`] that emits events forever and
///    records when its peer closes the TCP connection.
/// 2. Open a raw TCP client to ngx, send `GET /events` with
///    `Accept: text/event-stream`, read the response head and
///    the first event to confirm the stream is live.
/// 3. Drop the client side. This triggers
///    `session.write_response_body` error → `break` out of
///    the ngx body loop → `yamux_stream` drops → yamux sends
///    RST to the tun → tun's `tokio::io::copy` errors → tun
///    calls `backend.shutdown()` (the fix under test).
/// 4. Poll the backend's `peer_closed_at` for up to 2 s.
///    Assert it returned `Some` and that
///    `closed_at - t_drop < 1 s`.
///
/// Pre-fix this test would time out at 2 s — the backend TCP
/// would sit open until OS-level FIN timeout (60–120 s on
/// Linux) because the tun-side `copy` ignored yamux errors
/// and never called `backend.shutdown()`. The 1 s ceiling is
/// generous (1× scheduler tick + loopback RTT) but well below
/// the pre-fix OS timeout, so the test distinguishes clearly.
#[tokio::test]
async fn real_e2e_tunnel_sse_client_disconnect_propagates_to_backend() {
    let backend = ObservableSseBackend::start(Duration::from_millis(50)).await;
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

    // Open a raw TCP client and send an SSE GET. We do **not**
    // use `raw_sse_request` here because that helper reads
    // until EOF — we need to drop the client mid-stream.
    let mut client = tokio::net::TcpStream::connect(&addr)
        .await
        .expect("connect to ngx");
    let req = "GET /events HTTP/1.1\r\n\
               Host: sse.test\r\n\
               Connection: close\r\n\
               Accept: text/event-stream\r\n\
               User-Agent: pangolin-e2e-sse-disconnect\r\n\
               \r\n";
    client.write_all(req.as_bytes()).await.expect("write req");
    client.flush().await.expect("flush req");

    // Read the response head + the first event to confirm
    // the stream is live end-to-end (i.e. the tun actually
    // connected to the backend and started relaying).
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1];
    loop {
        match tokio::time::timeout(Duration::from_secs(3), client.read(&mut tmp)).await {
            Ok(Ok(0)) => panic!(
                "ngx closed before any bytes arrived; \
                 backend seen_request={}, ngx log:\n{}\n\ntun log:\n{}",
                backend.seen_request().await,
                ngx.log_string(),
                _tun.log_string()
            ),
            Ok(Ok(_)) => {
                buf.push(tmp[0]);
                if buf.ends_with(b"data: event 0") {
                    break;
                }
            }
            Ok(Err(e)) => panic!(
                "read error: {e}; backend seen_request={}, ngx log:\n{}\n\ntun log:\n{}",
                backend.seen_request().await,
                ngx.log_string(),
                _tun.log_string()
            ),
            Err(_) => panic!(
                "read head+event-0 timeout (3s); backend seen_request={}, partial={:?}\nngx log:\n{}\n\ntun log:\n{}",
                backend.seen_request().await,
                String::from_utf8_lossy(&buf),
                ngx.log_string(),
                _tun.log_string()
            ),
        }
    }

    // Drop the client. This is the disconnect event under test.
    let t_drop = Instant::now();
    drop(client);

    // Poll `peer_closed_at` for up to 2 s.
    let mut saw_close: Option<Instant> = None;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Some(t) = backend.peer_closed_at().await {
            saw_close = Some(t);
            break;
        }
    }
    let closed_at = saw_close.unwrap_or_else(|| {
        panic!(
            "backend TCP never closed within 2s after client drop. \
             This means the tun did not call backend.shutdown() — see \
             docs/design/sse-reconnect.md. ngx log:\n{}\n\ntun log:\n{}",
            ngx.log_string(),
            _tun.log_string()
        )
    });

    let elapsed = closed_at.duration_since(t_drop);
    assert!(
        elapsed < Duration::from_secs(1),
        "backend TCP took {elapsed:?} to close after client drop \
         (expected < 1s). Pre-fix behaviour was 60–120 s \
         (OS FIN timeout). ngx log:\n{}\n\ntun log:\n{}",
        ngx.log_string(),
        _tun.log_string()
    );
    eprintln!(
        "real_e2e_tunnel_sse_client_disconnect_propagates_to_backend: \
         backend TCP closed {elapsed:?} after client drop"
    );
}

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
        for (i, slot) in arrival.iter_mut().enumerate() {
            if slot.is_none() {
                let needle = format!("data: event {i}").into_bytes();
                if body_buf.windows(needle.len()).any(|w| w == needle) {
                    *slot = Some(sent_at.elapsed());
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

/// **SSE through a direct (non-tunnel) Http backend.**
///
/// Regression test: prior to the fix, the `is_streaming` short-
/// circuit in `request_filter` unconditionally called
/// `handle_streaming_request`, which returned 501 when the site
/// had no tunnel — even though pingora's direct path streams
/// H1/H2 chunked responses natively. Any site that wanted SSE
/// (chat streams, live tail of `/logs`, observability) was
/// forced to be fronted by a tun.
///
/// Post-fix: the `is_streaming` check now branches on
/// `tun_name.is_empty()` — for direct backends we fall through
/// to the standard direct path and let pingora stream the
/// response. This test pins that behavior so a future refactor
/// can't quietly regress it back to 501.
#[tokio::test]
async fn real_e2e_direct_sse_streams_through() {
    let backend = SseBackend::start(3, Duration::from_millis(50)).await;
    let backend_addr = backend.addr().to_string();

    // NOTE: backend is `http://...` (no `tunname:` prefix).
    // The site therefore has `tun_name.is_empty() == true`,
    // and SSE must take the direct path.
    let ngx = NgxProcess::start(move |db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        // No `seed_tun` call — there is no tun for this site.
        seed_site(&conn, "sse-direct-site", &format!("http://{backend_addr}"));
        seed_domain(&conn, "sse-direct.test", "sse-direct-site");
    })
    .await;

    // No `TunProcess::start` — there is no tun.
    let addr = format!("127.0.0.1:{}", ngx.http_port);

    let (status, headers, body) =
        raw_sse_request(&addr, "sse-direct.test", "/events", Duration::from_secs(2)).await;

    // Pre-fix: 501 (Not Implemented). Post-fix: 200.
    assert_eq!(
        status,
        200,
        "SSE request must stream through direct (non-tunnel) \
         backend, got {status}. headers={headers:?}\nbody={body:?}\
         \nngx log:\n{}",
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
        "SSE direct backend never received the request — the \
         request was either dropped or short-circuited. ngx log:\n{}",
        ngx.log_string()
    );
}

/// **Direct-path SSE: client disconnect propagates to backend.**
///
/// Mirrors [`real_e2e_tunnel_sse_client_disconnect_propagates_to_backend`]
/// but for the **direct** (non-tunnel) SSE path. The direct path
/// does not run our hand-written streaming loop; it falls through
/// to pingora's `tokio::try_join!` model
/// (`pingora-proxy/src/proxy_h1.rs:106-115`), which is supposed
/// to drop the upstream on client disconnect without any
/// pangolin-side code change. This test **locks that contract**:
/// if a future refactor of the direct SSE path breaks the
/// upstream-drop behaviour, this test will fail.
///
/// Pre-fix: this scenario was impossible (direct SSE returned
/// 501 — the bug fixed by PR #85). Post-#85 the path works, and
/// this test exercises its disconnect handling.
#[tokio::test]
async fn real_e2e_direct_sse_client_disconnect_propagates_to_backend() {
    let backend = ObservableSseBackend::start(Duration::from_millis(50)).await;
    let backend_addr = backend.addr().to_string();

    // NOTE: backend URL has no `tunname:` prefix → site has
    // `tun_name.is_empty() == true` → SSE takes the direct path.
    let ngx = NgxProcess::start(move |db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        // No `seed_tun` call.
        seed_site(&conn, "sse-direct-site", &format!("http://{backend_addr}"));
        seed_domain(&conn, "sse-direct.test", "sse-direct-site");
    })
    .await;

    let addr = format!("127.0.0.1:{}", ngx.http_port);

    let mut client = tokio::net::TcpStream::connect(&addr)
        .await
        .expect("connect to ngx");
    let req = "GET /events HTTP/1.1\r\n\
               Host: sse-direct.test\r\n\
               Connection: close\r\n\
               Accept: text/event-stream\r\n\
               User-Agent: pangolin-e2e-direct-sse-disconnect\r\n\
               \r\n";
    client.write_all(req.as_bytes()).await.expect("write req");
    client.flush().await.expect("flush req");

    // Read head + first event to confirm the stream is live.
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1];
    loop {
        match tokio::time::timeout(Duration::from_secs(3), client.read(&mut tmp)).await {
            Ok(Ok(0)) => panic!(
                "ngx closed before any bytes arrived on direct path; \
                 backend seen_request={}, ngx log:\n{}",
                backend.seen_request().await,
                ngx.log_string()
            ),
            Ok(Ok(_)) => {
                buf.push(tmp[0]);
                if buf.ends_with(b"data: event 0") {
                    break;
                }
            }
            Ok(Err(e)) => panic!(
                "direct-path read error: {e}; backend seen_request={}, ngx log:\n{}",
                backend.seen_request().await,
                ngx.log_string()
            ),
            Err(_) => panic!(
                "direct-path read head+event-0 timeout (3s); backend seen_request={}, partial={:?}\nngx log:\n{}",
                backend.seen_request().await,
                String::from_utf8_lossy(&buf),
                ngx.log_string()
            ),
        }
    }

    let t_drop = Instant::now();
    drop(client);

    let mut saw_close: Option<Instant> = None;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Some(t) = backend.peer_closed_at().await {
            saw_close = Some(t);
            break;
        }
    }
    let closed_at = saw_close.unwrap_or_else(|| {
        panic!(
            "direct-path backend TCP never closed within 2s after client drop. \
             This means pingora's upstream-drop behaviour was broken — see \
             docs/design/sse-reconnect.md (Direct path note). ngx log:\n{}",
            ngx.log_string()
        )
    });

    let elapsed = closed_at.duration_since(t_drop);
    assert!(
        elapsed < Duration::from_secs(1),
        "direct-path backend TCP took {elapsed:?} to close after client drop \
         (expected < 1s). Pre-#85 this scenario was untestable (501 short-circuit). \
         ngx log:\n{}",
        ngx.log_string()
    );
    eprintln!(
        "real_e2e_direct_sse_client_disconnect_propagates_to_backend: \
         backend TCP closed {elapsed:?} after client drop"
    );
}

/// **SSE through a direct (non-tunnel) Https backend.**
///
/// Mirrors the Https case from the screenshot: `dev.yaitoo.cn`
/// is a direct `http://127.0.0.1:8080` backend, and the browser
/// was hitting `https://dev.yaitoo.cn/sse` (H1 to ngx, H1 to
/// the local backend). The 501 came from the tun-only
/// short-circuit, not from TLS. This test exercises the same
/// routing decision with a *plain HTTP* local backend but under
/// a TLS-terminating edge (we use the raw H1 helper for the
/// internal connection; the bug and the fix are at the
/// request_filter level, identical between H1 and HTTPS edges).
#[tokio::test]
async fn real_e2e_direct_sse_via_https_edge() {
    let backend = SseBackend::start(2, Duration::from_millis(50)).await;
    let backend_addr = backend.addr().to_string();

    let ngx = NgxProcess::start(move |db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        // Direct Https backend (the dev.yaitoo.cn shape).
        seed_site(&conn, "sse-https-site", &format!("https://{backend_addr}"));
        seed_domain(&conn, "sse-https.test", "sse-https-site");
    })
    .await;

    // The local SSE backend speaks H1 — the TLS in the URL is
    // only there to exercise the `BackendTarget::Https { .. }`
    // branch. The backend will fail the TLS handshake (it
    // doesn't have a cert), so we expect a 5xx (502 from the
    // Https upstream peer). The bug we are regression-testing
    // is the *501*: if the test ever gets 501, the tun-only
    // short-circuit has come back. The strict `status >= 500`
    // assertion below is deliberate — it documents that
    // direct-Https-against-an-H1-backend is a server-side
    // failure (handshake error), not a "lucky 200" success.
    // 200 would mean we accidentally hit the mock backend
    // without doing TLS, which would be a test fixture
    // problem, not a real production outcome.
    let addr = format!("127.0.0.1:{}", ngx.http_port);
    let (status, headers, body) =
        raw_sse_request(&addr, "sse-https.test", "/events", Duration::from_secs(3)).await;

    assert_ne!(
        status,
        501,
        "SSE on direct Https must NOT 501 (regression of the \
         tunnel-only short-circuit). got {status}. \
         headers={headers:?}\nbody={body:?}\nngx log:\n{}",
        ngx.log_string()
    );
    // The TLS handshake will fail (the local mock backend is
    // H1), so the expected status is in the 5xx range (502
    // or 504). The point of this test is the 501 absence.
    assert!(
        status >= 500,
        "expected a 5xx from the TLS handshake failure, got \
         {status}. headers={headers:?}\nbody={body:?}\nngx log:\n{}",
        ngx.log_string()
    );
}

/// **SSE on a `file://` backend returns `415 Unsupported Media Type`**.
///
/// Locks in the contract from the code review: when a streaming
/// request (Accept: text/event-stream) hits a site whose backend
/// is `file://`, the proxy refuses with 415 + a JSON body that
/// names the exact error (`streaming_unsupported`) so a
/// programmatic client can distinguish it from a generic 400
/// and surface a helpful message. Regression test: if anyone
/// refactors `respond_streaming_unsupported_on_file` and
/// accidentally flips it back to a bare 400 (or to 200 with
/// the file contents), this test fails.
///
/// We do **not** need a real file on disk — `415` is decided
/// at `request_filter` time, before `serve_file_target` ever
/// opens the file.
#[tokio::test]
async fn real_e2e_file_sse_returns_415_with_json_body() {
    let ngx = NgxProcess::start(move |db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        // `file://` backend (no tun).
        seed_site(&conn, "sse-file-site", "file:///tmp/pangolin-test-docroot");
        seed_domain(&conn, "sse-file.test", "sse-file-site");
    })
    .await;

    let addr = format!("127.0.0.1:{}", ngx.http_port);
    let (status, headers, body) =
        raw_sse_request(&addr, "sse-file.test", "/anything", Duration::from_secs(2)).await;

    // Must be 415 (not 400, not 200, not 501).
    assert_eq!(
        status,
        415,
        "SSE on file:// must be 415 Unsupported Media Type, \
         got {status}. headers={headers:?}\nbody={body:?}\nngx log:\n{}",
        ngx.log_string()
    );
    let lower_headers = headers.to_ascii_lowercase();
    assert!(
        lower_headers.contains("content-type: application/json"),
        "415 body must be JSON for programmatic clients to branch \
         on the `error` field, got headers: {headers:?}"
    );
    // JSON body must name the exact error so callers don't have
    // to string-match the human message.
    assert!(
        body.contains("\"error\":\"streaming_unsupported\""),
        "415 JSON body must include \
         `error: streaming_unsupported`, got body: {body:?}"
    );
}

/// **SSE handler is shutdown-aware — a long-lived `/api/logs/stream`
/// connection drops within 1.5 s of SIGINT, not 5–10 s.**
///
/// Regression test for the Ctrl-C latency report from the
/// user. Pre-fix, the SSE access-log handler's broadcast loop
/// awaited `rx.recv()` indefinitely; an idle SSE client
/// connected during a SIGINT would keep the `pangolin-http`
/// runtime pinned for pingora's full
/// `graceful_shutdown_timeout_seconds` (5 s) before the runtime
/// could be torn down — the operator-facing shutdown latency
/// jumped to `grace_period + graceful_shutdown_timeout` (≈10 s)
/// instead of `grace_period` alone (≈5 s). Visible in the log
/// as `Waiting for service runtime pangolin-http to exit`.
///
/// Post-fix the handler `select!`s on
/// `pingora::server::ShutdownWatch::changed()` inside the
/// broadcast loop and breaks with a `: shutdown\n\n` SSE
/// comment the moment the runtime's flag flips. The TCP
/// connection drops almost immediately — the assertion below
/// is `1.5 s` (generous: 500 ms handler exit + 500 ms OS /
/// pingora dance + 500 ms scheduler slack). The pre-fix
/// behaviour would be `5+ s` and would fail this test.
///
/// ## Why we take `ngx.child` out of the harness
///
/// `NgxProcess::drop` sends `SIGTERM` then `SIGKILL` if the
/// child is still alive. We want this test to observe the
/// **SIGINT** shutdown path end-to-end (the child needs to be
/// allowed to drain gracefully, not yanked out of the
/// runtime). Taking the child out of the `Option<Child>` moves
/// the Drop-skip into our hands, where we wait the full
/// graceful-drain time. If the test fails and the child is
/// still alive, the harness's `SIGTERM` + `SIGKILL` will
/// eventually clean it up.
#[tokio::test]
async fn real_e2e_sse_drops_connection_promptly_on_sigint() {
    let mut ngx = NgxProcess::start(|db_path| {
        init_pangolin_db(db_path);
    })
    .await;

    // 0) Authenticate against the admin port to get a session
    //    cookie. `/api/logs/stream` is admin-only.
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

    // 1) Open the SSE connection and confirm the handshake
    //    (200 OK + text/event-stream). We deliberately do NOT
    //    trigger any access-log entries before SIGINT — an idle
    //    SSE client is the worst case for the pre-fix bug,
    //    because the broadcast loop has no entry to wake on.
    let sse_addr = format!("127.0.0.1:{}", ngx.admin_port);
    let mut sse_stream = tokio::net::TcpStream::connect(&sse_addr)
        .await
        .expect("connect admin port");
    let req = format!(
        "GET /api/logs/stream HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Accept: text/event-stream\r\n\
         Connection: close\r\n\
         User-Agent: pangolin-sse-shutdown-e2e\r\n\
         Cookie: {session_cookie}\r\n\
         \r\n"
    );
    sse_stream
        .write_all(req.as_bytes())
        .await
        .expect("write sse req");
    sse_stream.flush().await.expect("flush sse req");

    // Read until we see \r\n\r\n. The handler should respond
    // with `200 OK` and `Content-Type: text/event-stream`
    // before entering the broadcast loop.
    let mut hdr_buf = Vec::new();
    let mut tmp = [0u8; 1];
    loop {
        match timeout(Duration::from_secs(2), sse_stream.read(&mut tmp)).await {
            Ok(Ok(0)) => panic!("EOF before SSE handshake complete"),
            Ok(Ok(_)) => {
                hdr_buf.push(tmp[0]);
                if hdr_buf.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            Ok(Err(e)) => panic!("SSE handshake read error: {e}"),
            Err(_) => panic!("SSE handshake timeout (2s)"),
        }
    }
    let hdr_str = String::from_utf8_lossy(&hdr_buf).into_owned();
    assert_eq!(
        hdr_str.lines().next().unwrap_or(""),
        "HTTP/1.1 200 OK",
        "SSE handshake first line: {hdr_str:?}"
    );
    assert!(
        hdr_str
            .to_ascii_lowercase()
            .contains("content-type: text/event-stream"),
        "SSE handshake missing text/event-stream: {hdr_str:?}"
    );

    // 2) Take the child out of the harness so the harness Drop
    //    does not race our SIGINT with its own SIGTERM+SIGKILL.
    let mut child = ngx.child.take().expect("child process handle");
    let pid = child.id().expect("child pid") as i32;

    // 3) Send SIGINT and time the response. We expect the
    //    connection to drop within 1.5 s. The pre-fix
    //    behaviour was 5–10 s, so the assertion margin is
    //    large enough to be robust on a slow CI box but
    //    small enough to catch the regression.
    let t_sigint = Instant::now();
    // SAFETY: libc::kill with a valid pid and a standard
    // signal number has no memory-safety implications for
    // the caller.
    let rc = unsafe { libc::kill(pid, libc::SIGINT) };
    assert_eq!(
        rc,
        0,
        "kill(SIGINT) returned {rc} (errno: {})",
        std::io::Error::last_os_error()
    );

    // 4) Read from the SSE socket. With the fix, the handler
    //    emits `: shutdown\n\n` and finishes the response
    //    within ~100 ms; the kernel-side close arrives a bit
    //    later. We bound the wait at 1.5 s.
    let mut body_buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let conn_dropped = loop {
        match timeout(Duration::from_millis(1500), sse_stream.read(&mut tmp)).await {
            Ok(Ok(0)) => break true, // EOF — server closed cleanly
            Ok(Ok(n)) => {
                body_buf.extend_from_slice(&tmp[..n]);
                // Keep reading until EOF or we see the
                // : shutdown comment (a non-essential early
                // signal that the fix is in place).
                continue;
            }
            Ok(Err(_)) => break true, // read error counts as drop
            Err(_) => break false,    // 1.5 s elapsed — still alive
        }
    };
    let elapsed = t_sigint.elapsed();
    assert!(
        conn_dropped,
        "SSE connection was NOT closed within 1.5 s of SIGINT \
         (took >{elapsed:?}). This is the pre-fix behaviour: the \
         broadcast loop is not racing the ShutdownWatch and the \
         runtime is pinned until graceful_shutdown_timeout expires. \
         The fix lives in crates/ngx/src/sse.rs::handle_access_log_stream \
         (the `select!` on `shutdown.changed()`). \
         body so far: {body_buf:?}\nngx log:\n{}",
        ngx.log_string()
    );
    assert!(
        elapsed < Duration::from_millis(1500),
        "SSE connection did drop, but only after {elapsed:?} — \
         the 1.5 s budget is the pre-fix baseline; anything close \
         to 5 s is a regression. body={body_buf:?}"
    );

    // 5) Wait for the child to exit cleanly. We give it the
    //    full graceful-drain budget (12 s — covers pingora's
    //    5 s grace + 5 s runtime drain + 2 s scheduler slack)
    //    so the test does not race the harness Drop.
    let drain_deadline = Instant::now() + Duration::from_secs(12);
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) => {
                if Instant::now() >= drain_deadline {
                    let _ = child.start_kill();
                    panic!(
                        "ngx child did not exit within 12 s of SIGINT; \
                         killing it. ng x log:\n{}",
                        ngx.log_string()
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("try_wait error: {e}"),
        }
    }
    // The child should have exited via the SIGINT graceful
    // path — a 0 exit code would mean the test environment
    // somehow had no shutdown signalled; we don't assert on
    // the specific code, just that the child is gone.
}
