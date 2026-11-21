//! End-to-end tests for the tunnel's **transports** (issue #153).
//!
//! The tunnel is a splice: the gateway's pingora writes the client's
//! request bytes into a tunnel stream and the tun copies them to the
//! backend. Two transports carry those bytes — QUIC (primary) and
//! WebSocket over TCP (fallback) — and every test here pins the
//! transport, because a test that cannot pin the transport cannot tell
//! a QUIC session from a fallback session that silently happened.
//!
//! The tests drive the **real** `pangolin-ngx` and `pangolin-tun`
//! binaries.
//!
//! **What this suite asserts, and what it leaves to the crate.**
//! The tun's own decisions — which transport it picked, whether it
//! fell back, how it reported a refused gateway — belong to the
//! `pangolin-tun` crate and are asserted by the crate's own
//! end-to-end suite. This suite asserts the **product path**: the
//! request reaching the backend byte-exact, the endless response
//! arriving, the WebSocket upgrade round-tripping — and, on the
//! gateway's side, the transport the gateway itself logged. The tun's
//! log is the tun's; the gateway's log is the gateway's.

use std::sync::Arc;
use std::time::Duration;

use rusqlite::Connection;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use crate::harness::{NgxProcess, TunProcess, init_pangolin_db};

// ---------------------------------------------------------------------------
// Mock backends
// ---------------------------------------------------------------------------

/// A backend that records the request it saw and answers with a
/// fixed JSON body.
struct EchoBackend {
    addr: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl EchoBackend {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let reqs_for_task = requests.clone();
        tokio::spawn(async move {
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
                    reqs.lock().await.push(req);
                    let response = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                                    Content-Length: 15\r\nConnection: close\r\n\r\n\
                                    {\"method\":\"GET\"}";
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.flush().await;
                });
            }
        });
        Self { addr, requests }
    }

    fn addr(&self) -> &str {
        &self.addr
    }

    async fn seen(&self) -> Vec<String> {
        self.requests.lock().await.clone()
    }
}

/// A backend that emits a fixed number of SSE events and then closes.
struct SseBackend {
    addr: String,
    seen_request: Arc<Mutex<bool>>,
}

impl SseBackend {
    async fn start(events: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let seen_request = Arc::new(Mutex::new(false));
        let seen_for_task = seen_request.clone();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let seen = seen_for_task.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    if stream.read(&mut buf).await.is_err() {
                        return;
                    }
                    *seen.lock().await = true;
                    let mut head = String::from(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                         Cache-Control: no-cache\r\nConnection: close\r\n\r\n",
                    );
                    for i in 0..events {
                        head.push_str(&format!("data: event {i}\n\n"));
                    }
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.flush().await;
                });
            }
        });
        Self { addr, seen_request }
    }

    fn addr(&self) -> &str {
        &self.addr
    }

    async fn seen_request(&self) -> bool {
        *self.seen_request.lock().await
    }
}

/// A backend that speaks the WebSocket handshake and then echoes
/// every message it receives.
///
/// The server side uses tungstenite, so the frames on the wire are
/// real RFC 6455 frames — the test exercises the bytes the tunnel
/// actually carries.
struct WsBackend {
    addr: String,
}

impl WsBackend {
    async fn start() -> Self {
        use futures_util::{SinkExt, StreamExt};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut ws = match tokio_tungstenite::accept_async(stream).await {
                        Ok(ws) => ws,
                        Err(_) => return,
                    };
                    while let Some(Ok(message)) = ws.next().await {
                        if message.is_close() {
                            break;
                        }
                        if ws.send(message).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        Self { addr }
    }

    fn addr(&self) -> &str {
        &self.addr
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn seed_tun(conn: &Connection, name: &str, enabled: bool) {
    // `db::sha256_hex` is the helper production derives `tun.token_hash`
    // with (issue #39) — see the note in `sse_e2e.rs`.
    let hash = pangolin_core::db::sha256_hex("test-token");
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO tun (name, token, token_hash, enabled, online, registered_at, last_seen_at)
         VALUES (?1, ?2, ?3, ?4, 0, ?5, ?5)",
        rusqlite::params![name, "test-token", hash, enabled as i32, now],
    )
    .expect("insert tun");
}

fn seed_site(conn: &Connection, name: &str, backend: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO sites (name, backend, enabled, host_mode, host_custom, created_at, updated_at)
         VALUES (?1, ?2, 1, 'passthrough', NULL, ?3, ?3)",
        rusqlite::params![name, backend, now],
    )
    .expect("insert site");
}

fn seed_domain(conn: &Connection, domain: &str, site_name: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO domains (domain, site_name, enabled, created_at) VALUES (?1, ?2, 1, ?3)",
        rusqlite::params![domain, site_name, now],
    )
    .expect("insert domain");
}

/// Seed the `certs` row so the in-memory `CertLinkCache` has a link
/// for the domain — the TLS listener resolves the SNI through it. The
/// certificate blob itself is written to the cert dir afterwards.
fn seed_cert(conn: &Connection, domain: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    let sans_json = serde_json::to_string(&vec![domain.to_string()]).expect("serialize sans");
    conn.execute(
        "INSERT INTO certs (domain, cert_file, key_file, created_at, sans, source, status)
         VALUES (?1, '/dev/null', '/dev/null', ?2, ?3, 'manual', 'issued')",
        rusqlite::params![domain, now, sans_json],
    )
    .expect("insert cert");
}

/// Issue a raw HTTP/1.1 request and return the status and the body.
async fn raw_request(
    addr: &str,
    host: &str,
    method: &str,
    path: &str,
    body: &[u8],
) -> (u16, String) {
    crate::harness::raw_request(addr, host, method, path, body).await
}

/// Assert that a captured log contains `expected`.
///
/// The log is captured asynchronously — a detached task drains the
/// process's pipes — so a line the process has just written may not
/// have been read yet when the response arrives. Asserting on the log
/// directly is a race; poll for the line instead.
///
/// Shared with `real_e2e.rs`: the gateway's log is asserted from
/// both files.
pub(crate) async fn assert_log_contains(
    log: &std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    what: &str,
    expected: &str,
) {
    assert!(
        crate::harness::wait_for_log(log, expected, std::time::Duration::from_secs(5)).await,
        "the {what} log must contain {expected:?}, got:\n{}",
        String::from_utf8_lossy(&log.lock().unwrap())
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A normal request through a tun pinned to **QUIC**.
///
/// This is the primary transport: one QUIC connection per tun, one
/// bidirectional stream per request. The test pins the transport so a
/// silent fallback cannot make it pass.
#[tokio::test]
async fn real_e2e_tunnel_quic_http_request() {
    let backend = EchoBackend::start().await;
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

    let tun = TunProcess::start_with_transport(&ngx, "office", "test-token", "quic").await;

    let addr = format!("127.0.0.1:{}", ngx.http_port);
    let (status, body) = raw_request(&addr, "office.test", "GET", "/api/echo?x=1", b"").await;

    assert_eq!(
        status,
        200,
        "expected 200 from the backend through the QUIC tunnel, got {status} \
         (body={body:?})\nngx log:\n{}\ntun log:\n{}",
        ngx.log_string(),
        tun.log_string()
    );

    // The backend must have seen the request — the gateway's request
    // bytes have to reach the tun unchanged.
    let seen = backend.seen().await;
    assert_eq!(
        seen.len(),
        1,
        "the backend should have seen exactly one request, saw {}",
        seen.len()
    );
    assert!(
        seen[0].starts_with("GET /api/echo?x=1 HTTP/1.1"),
        "the request line must survive the splice byte-exact, got {:?}",
        seen[0]
    );

    // The gateway must report that the session came up over QUIC.
    assert_log_contains(&ngx.log, "gateway", "connected over quic").await;
}

/// A tunnelled backend whose URL carries a **path prefix** must get
/// the prefixed path.
///
/// `office:http://10.0.0.1:8080/apis` means "the backend serves
/// under `/apis`". The pre-#153 tunnel path joined the prefix onto
/// the request path before handing the request to the tun; the splice
/// carries the request bytes verbatim, so the prefix has to be
/// applied on the gateway.
#[tokio::test]
async fn real_e2e_tunnel_backend_path_prefix_is_applied() {
    let backend = EchoBackend::start().await;
    let backend_addr = backend.addr().to_string();

    let ngx = NgxProcess::start(move |db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        seed_tun(&conn, "office", true);
        seed_site(
            &conn,
            "office-site",
            &format!("office:http://{backend_addr}/apis"),
        );
        seed_domain(&conn, "office.test", "office-site");
    })
    .await;

    let tun = TunProcess::start_with_transport(&ngx, "office", "test-token", "quic").await;

    let addr = format!("127.0.0.1:{}", ngx.http_port);
    let (status, body) = raw_request(&addr, "office.test", "GET", "/api/echo?x=1", b"").await;
    assert_eq!(
        status,
        200,
        "expected 200 through the tun, got {status} (body={body:?})\nngx log:\n{}\ntun log:\n{}",
        ngx.log_string(),
        tun.log_string()
    );

    let seen = backend.seen().await;
    assert_eq!(seen.len(), 1, "the backend should have seen one request");
    assert!(
        seen[0].starts_with("GET /apis/api/echo?x=1 HTTP/1.1"),
        "the backend's path prefix must be applied to the request path, got {:?}",
        seen[0]
    );
}

/// The same request through a tun pinned to **TCP** — the
/// WebSocket/TCP fallback transport.
#[tokio::test]
async fn real_e2e_tunnel_tcp_http_request() {
    let backend = EchoBackend::start().await;
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

    let tun = TunProcess::start_with_transport(&ngx, "office", "test-token", "tcp").await;

    let addr = format!("127.0.0.1:{}", ngx.http_port);
    let (status, body) = raw_request(&addr, "office.test", "GET", "/api/echo?x=1", b"").await;

    assert_eq!(
        status,
        200,
        "expected 200 from the backend through the TCP tunnel, got {status} \
         (body={body:?})\nngx log:\n{}\ntun log:\n{}",
        ngx.log_string(),
        tun.log_string()
    );

    let seen = backend.seen().await;
    assert_eq!(seen.len(), 1, "the backend should have seen one request");

    // The gateway must report that the session came up over the
    // WebSocket/TCP transport.
    assert_log_contains(
        &ngx.log,
        "gateway",
        "connected over the WebSocket/TCP transport",
    )
    .await;
}

/// An endless response (SSE) through a tun pinned to **QUIC**.
///
/// The SSE path used to need a hand-written byte-relay because the
/// tun's buffered `HttpResponse` path deadlocked on an infinite
/// chunked body. With the splice, pingora streams the response
/// natively — so the events have to arrive as they are produced.
#[tokio::test]
async fn real_e2e_tunnel_quic_sse_streams_through() {
    sse_streams_through("quic").await;
}

/// The same endless response over the **WebSocket/TCP** transport.
///
/// The fallback transport has to carry a streaming response just as
/// well — the splice does not care which transport the bytes travel
/// on.
#[tokio::test]
async fn real_e2e_tunnel_tcp_sse_streams_through() {
    sse_streams_through("tcp").await;
}

async fn sse_streams_through(transport: &str) {
    let backend = SseBackend::start(3).await;
    let backend_addr = backend.addr().to_string();

    let ngx = NgxProcess::start(move |db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        seed_tun(&conn, "sse", true);
        seed_site(&conn, "sse-site", &format!("sse:http://{backend_addr}"));
        seed_domain(&conn, "sse.test", "sse-site");
    })
    .await;

    let tun = TunProcess::start_with_transport(&ngx, "sse", "test-token", transport).await;

    let addr = format!("127.0.0.1:{}", ngx.http_port);
    let (status, headers, body) =
        crate::sse_e2e::raw_sse_request(&addr, "sse.test", "/events", Duration::from_secs(5)).await;

    assert_eq!(
        status,
        200,
        "the SSE request must return 200 through the QUIC tunnel, got {status} \
         (headers={headers:?})\nbody={body:?}\nngx log:\n{}\ntun log:\n{}",
        ngx.log_string(),
        tun.log_string()
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
            "the SSE body is missing event {i}, got: {body:?}"
        );
    }
    assert!(
        backend.seen_request().await,
        "the SSE backend never received the request"
    );
    // The gateway must report the pinned transport. The tun's own
    // "over <transport>" line is the crate's business; the
    // gateway's is this suite's — and it is what keeps the pin
    // visible now that the tun's log is not asserted here.
    if transport == "quic" {
        assert_log_contains(&ngx.log, "gateway", "connected over quic").await;
    } else {
        assert_log_contains(
            &ngx.log,
            "gateway",
            "connected over the WebSocket/TCP transport",
        )
        .await;
    }
}

/// A WebSocket upgrade through a tun pinned to **QUIC**.
///
/// The upgrade used to need a hand-written relay on the tun side
/// (`handle_ws_upgrade`) that replayed the request against the
/// backend. With the splice the tun copies the bytes and does not
/// know what they are — so the upgrade has to work without any
/// special handling on that side.
#[tokio::test]
async fn real_e2e_tunnel_quic_ws_upgrade() {
    ws_upgrade_over("quic").await;
}

/// The same WebSocket upgrade over the **WebSocket/TCP** transport.
#[tokio::test]
async fn real_e2e_tunnel_tcp_ws_upgrade() {
    ws_upgrade_over("tcp").await;
}

async fn ws_upgrade_over(transport: &str) {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let backend = WsBackend::start().await;
    let backend_addr = backend.addr().to_string();

    let ngx = NgxProcess::start(move |db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open db");
        seed_tun(&conn, "ws", true);
        seed_site(&conn, "ws-site", &format!("ws:http://{backend_addr}"));
        seed_domain(&conn, "ws.test", "ws-site");
    })
    .await;

    let tun = TunProcess::start_with_transport(&ngx, "ws", "test-token", transport).await;

    // The proxy routes by the `Host` header, so the upgrade request
    // has to carry the public domain rather than the address the
    // client dialled.
    use tokio_tungstenite::client_async;
    use tokio_tungstenite::tungstenite::handshake::client::Request;
    let url = format!("ws://127.0.0.1:{}/chat", ngx.http_port);
    let request = Request::builder()
        .method("GET")
        .uri(&url)
        .header("Host", "ws.test")
        .header("Upgrade", "websocket")
        .header("Connection", "Upgrade")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
        .body(())
        .expect("build the WS request");
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", ngx.http_port))
        .await
        .expect("tcp connect");
    let (mut socket, response) = client_async(request, tcp).await.expect("ws connect");
    assert_eq!(
        response.status().as_u16(),
        101,
        "the WS upgrade must succeed with 101 Switching Protocols"
    );

    socket
        .send(Message::Text("hello".into()))
        .await
        .expect("send ws frame");
    let echoed = match socket.next().await {
        Some(Ok(frame)) => frame,
        other => panic!(
            "ws frame: {other:?}\nngx log:\n{}\ntun log:\n{}",
            ngx.log_string(),
            tun.log_string()
        ),
    };
    let text = match echoed {
        Message::Text(t) => t,
        Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
        other => panic!("unexpected ws frame: {other:?}"),
    };
    assert_eq!(
        text, "hello",
        "the WS payload must round-trip through the tunnel byte-exact"
    );
    // The gateway must report the pinned transport — see the same
    // assertion in `sse_streams_through` for why it is the
    // gateway's log and not the tun's.
    if transport == "quic" {
        assert_log_contains(&ngx.log, "gateway", "connected over quic").await;
    } else {
        assert_log_contains(
            &ngx.log,
            "gateway",
            "connected over the WebSocket/TCP transport",
        )
        .await;
    }
}

/// A tun whose **QUIC attempt fails** falls back to the
/// WebSocket/TCP transport and still serves the request.
///
/// The gateway's QUIC listener cannot bind (the UDP port is occupied
/// by a socket that never answers), so the QUIC handshake times out
/// and the tun falls back. The test asserts that the request still
/// works — a tun that gave up instead of falling back would fail
/// that — and that the gateway reports its own degraded mode.
///
/// The tun's decision to fall back is the crate's behaviour and is
/// asserted by `pangolin-tun`'s own suite; the gateway's log line is
/// the gateway's, and stays here.
#[tokio::test]
async fn real_e2e_tunnel_quic_falls_back_to_tcp() {
    let backend = EchoBackend::start().await;
    let backend_addr = backend.addr().to_string();

    let ngx = NgxProcess::start_with_quic_blocked(move |db_path| {
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

    // The tun uses its default selector (`auto`): it tries QUIC and
    // falls back when the attempt fails.
    let tun = TunProcess::start(&ngx, "office", "test-token").await;

    let addr = format!("127.0.0.1:{}", ngx.http_port);
    let (status, body) = raw_request(&addr, "office.test", "GET", "/api/echo", b"").await;

    assert_eq!(
        status,
        200,
        "the request must be served over the fallback transport, got {status} \
         (body={body:?})\nngx log:\n{}\ntun log:\n{}",
        ngx.log_string(),
        tun.log_string()
    );

    // The gateway must report that it could not bind QUIC and is
    // serving the WebSocket/TCP tunnel only. Whether the tun *chose*
    // to fall back is the crate's business — `pangolin-tun`'s own
    // suite asserts that (`tests/fallback.rs`).
    assert_log_contains(&ngx.log, "gateway", "serving the WebSocket/TCP tunnel only").await;
}

/// An **h2** request through a tun pinned to **QUIC** reaches the
/// backend and succeeds.
///
/// The QUIC transport carries one bidirectional stream per request and
/// no multiplexer in between, so the `tokio-yamux` race that forced
/// every tunnel-backed vhost onto HTTP/1.1 cannot happen there — the
/// gateway may negotiate h2 for a QUIC-tunneled vhost (issue #210).
///
/// The client is `curl --http2`: it offers h2 in the ALPN extension
/// and uses whatever the server picks. `%{http_version}` reports the
/// protocol that was actually negotiated, so a `2` means the
/// handshake really was h2 — not a silent fallback to h1.
///
/// The **backend** still sees HTTP/1.1: the gateway terminates the
/// client's h2 and writes the request into the tunnel stream in
/// HTTP/1.1, because the tunnel is a byte splice.
#[tokio::test]
async fn real_e2e_tunnel_quic_negotiates_h2() {
    use std::process::Stdio;

    let backend = EchoBackend::start().await;
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
        // The TLS listener resolves the SNI through the cert link, so
        // the cert row has to exist before the gateway starts.
        seed_cert(&conn, "office.test");
    })
    .await;

    let tun = TunProcess::start_with_transport(&ngx, "office", "test-token", "quic").await;

    // A self-signed certificate for the vhost, in the autocert blob
    // layout the SNI callback reads.
    crate::harness::gen_self_signed(&["office.test"], "office.test", &ngx.cert_dir());

    let url = format!("https://office.test:{}/api/echo?x=1", ngx.tls_port);
    let output = tokio::process::Command::new("curl")
        .arg("--http2")
        .arg("--insecure")
        .arg("--max-time")
        .arg("10")
        .arg("--silent")
        .arg("--show-error")
        .arg("--output")
        .arg("-")
        .arg("--write-out")
        .arg("HTTP_CODE:%{http_code}\nHTTP_VERSION:%{http_version}\n")
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
        "an h2 request through a QUIC tunnel must reach the backend, got {status} \
         (body={body:?})\nngx log:\n{}\ntun log:\n{}",
        ngx.log_string(),
        tun.log_string()
    );

    // The negotiated protocol — the point of the change.
    let version = meta
        .split_once("HTTP_VERSION:")
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_default();
    assert_eq!(
        version,
        "2",
        "a QUIC-tunneled vhost must negotiate h2; the client got {version:?} \
         instead. stderr:\n{stderr}\nngx log:\n{}",
        ngx.log_string()
    );

    // The request reached the backend through the splice. The tunnel
    // carries HTTP/1.1 bytes — the gateway terminated the client's h2.
    let seen = backend.seen().await;
    assert_eq!(
        seen.len(),
        1,
        "the backend should have seen exactly one request, saw {}",
        seen.len()
    );
    assert!(
        seen[0].starts_with("GET /api/echo?x=1 HTTP/1.1"),
        "the request line must survive the splice byte-exact, got {:?}",
        seen[0]
    );

    // The gateway must report that the session came up over QUIC —
    // the transport the h2 decision was made on.
    assert_log_contains(&ngx.log, "gateway", "connected over quic").await;
}
