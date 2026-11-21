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
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;

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

/// What the backend saw on the connection the tun opened.
#[derive(Debug, Clone)]
struct BackendSeen {
    /// The request bytes the backend received.
    request: String,
    /// The status the backend answered.
    answered: u16,
}

/// A backend that answers `400 Bad Request` when the tunnel cuts the
/// exchange short.
///
/// It reads until the peer stops sending and answers then.  The
/// answer is the one a real server gives to a request it could not
/// read to the end — and the answer the gateway must **not** relay
/// to the client.
struct TruncatingBackend {
    addr: String,
    /// What the backend saw, once its read ended.
    seen: Arc<Mutex<Option<BackendSeen>>>,
    /// Fires when the backend has the first bytes of a request.
    arrived: Arc<Notify>,
    /// Fires when the backend has answered.
    answered: Arc<Notify>,
}

impl TruncatingBackend {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let seen: Arc<Mutex<Option<BackendSeen>>> = Arc::new(Mutex::new(None));
        let arrived = Arc::new(Notify::new());
        let answered = Arc::new(Notify::new());
        let seen_for_task = seen.clone();
        let arrived_for_task = arrived.clone();
        let answered_for_task = answered.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let seen = seen_for_task.clone();
                let arrived = arrived_for_task.clone();
                let answered = answered_for_task.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    loop {
                        let mut buf = [0u8; 4096];
                        match stream.read(&mut buf).await {
                            Ok(0) => break,
                            Ok(n) => {
                                request.extend_from_slice(&buf[..n]);
                                arrived.notify_one();
                            }
                            Err(_) => break,
                        }
                    }
                    let body = "missing required Host header";
                    let answer = format!(
                        "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain\r\n\
                         Content-Length: {len}\r\nConnection: close\r\n\r\n{body}",
                        len = body.len(),
                        body = body,
                    );
                    let _ = stream.write_all(answer.as_bytes()).await;
                    let _ = stream.flush().await;
                    answered.notify_one();
                    *seen.lock().await = Some(BackendSeen {
                        request: String::from_utf8_lossy(&request).into_owned(),
                        answered: 400,
                    });
                });
            }
        });
        Self {
            addr,
            seen,
            arrived,
            answered,
        }
    }

    fn addr(&self) -> &str {
        &self.addr
    }

    /// Wait until the backend has the first bytes of a request.
    async fn wait_for_request(&self) {
        self.arrived.notified().await;
    }

    /// Wait until the backend has answered.
    async fn wait_for_answer(&self) {
        self.answered.notified().await;
    }

    /// What the backend saw, once its read ended.
    async fn seen(&self) -> Option<BackendSeen> {
        self.seen.lock().await.clone()
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

/// A TCP forwarder between the tun and the gateway's tunnel port.
///
/// The tunnel session lives on the TCP connection the tun opens to the
/// gateway.  Cutting that connection is how a test kills a session
/// while the **tun process stays alive**: the tun reconnects on its
/// own, and the request that was in flight keeps the failure of the
/// session it belonged to.
struct TunnelProxy {
    /// The address the tun dials.
    addr: String,
    /// The copy tasks, one per connection.  A task owns both of the
    /// connection's sockets, so aborting it drops them and cuts the
    /// connection.
    tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl TunnelProxy {
    /// Forward to `upstream` and start listening.
    async fn start(upstream: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let tasks: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        let tasks_for_task = tasks.clone();
        tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let upstream = upstream.clone();
                let tasks = tasks_for_task.clone();
                let handle = tokio::spawn(async move {
                    let Ok(server) = TcpStream::connect(&upstream).await else {
                        return;
                    };
                    let mut from_client = client;
                    let mut from_server = server;
                    let _ = tokio::io::copy_bidirectional(&mut from_client, &mut from_server).await;
                });
                tasks.lock().await.push(handle);
            }
        });
        Self { addr, tasks }
    }

    fn addr(&self) -> &str {
        &self.addr
    }

    /// Cut every connection the proxy is carrying.
    ///
    /// Both ends go down with the task, so the tun and the gateway
    /// each see their half of the tunnel connection die.
    async fn cut(&self) {
        let tasks: Vec<JoinHandle<()>> = std::mem::take(&mut *self.tasks.lock().await);
        for task in tasks {
            task.abort();
        }
    }
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

/// A tunnel session that dies mid-request is an **error** on that
/// request, not a relayed backend answer and not a half-delivered
/// response.
///
/// The tun's connection is cut while the request is in flight.  Three
/// things have to hold:
///
/// * the client gets an error the **gateway** produced — not a
///   response that stops half-way;
/// * the backend's answer to the cut-short request never reaches the
///   client;
/// * the gateway sees the death of the session as an **error**.  The
///   tunnel's byte stream ends when the session dies, and an
///   end-of-stream says "the tun finished sending" — that is what
///   lets the tun hand the backend a request that was cut short.
///
/// The **gateway's log** is what pins the third one: the bytes on
/// the wire are the same either way — a dead session and a finished
/// one both reach pingora as the end of the response — so the client
/// cannot tell them apart.  The crate reports the death as an error,
/// and the gateway says so.
#[tokio::test]
async fn real_e2e_tunnel_a_dead_session_is_reported_as_an_error() {
    let backend = TruncatingBackend::start().await;
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

    // The tun reaches the gateway through a proxy: the test cuts the
    // tunnel's connection there, with the tun itself still running —
    // the tun is the process that has to notice the death.
    //
    // The transport is pinned to `tcp`: the WebSocket/TCP fallback is
    // the one that carries a yamux session, and a yamux session is
    // what reports the end of a stream.  The QUIC transport never
    // touches yamux, so it cannot show this failure at all.
    let proxy = TunnelProxy::start(format!("127.0.0.1:{}", ngx.tunnel_port)).await;
    let tun =
        TunProcess::start_with_transport_at(proxy.addr(), "office", "test-token", "tcp").await;

    // The request goes out; the backend has it and waits.
    let addr = format!("127.0.0.1:{}", ngx.http_port);
    let request =
        tokio::spawn(
            async move { raw_request(&addr, "office.test", "GET", "/api/echo", b"").await },
        );
    // The request has to reach the backend before the tunnel dies, or
    // the test would prove nothing.  The wait is bounded: a request
    // that never arrives must fail the test, not hang it.
    tokio::time::timeout(Duration::from_secs(5), backend.wait_for_request())
        .await
        .expect("the request must reach the backend");

    // The tunnel's connection dies while the request is in flight.
    proxy.cut().await;

    let (status, body) = request.await.expect("the request must not panic");

    assert!(
        status >= 500,
        "a dying tunnel session must be an error the gateway produced, got {status} \
         (body={body:?})\nngx log:\n{}\ntun log:\n{}",
        ngx.log_string(),
        tun.log_string()
    );
    assert!(
        !body.contains("missing required Host header"),
        "the backend's answer to the cut-short request must not be relayed to the \
         client, got {body:?}\nngx log:\n{}\ntun log:\n{}",
        ngx.log_string(),
        tun.log_string()
    );

    // The backend did answer — its `400` is the answer a dying
    // session used to hand to the client.
    tokio::time::timeout(Duration::from_secs(5), backend.wait_for_answer())
        .await
        .expect("the backend must answer");
    let seen = backend
        .seen()
        .await
        .expect("the backend must have seen the request");
    assert!(
        seen.request.starts_with("GET /api/echo HTTP/1.1"),
        "the request must have reached the backend before the tunnel died, got {:?}",
        seen.request
    );
    assert_eq!(seen.answered, 400, "the backend answers 400");

    // The gateway must see the death of the session as an error, not
    // as the end of the stream — see `copy_half` in
    // `crates/ngx/src/tunnel/connector.rs`.
    assert_log_contains(&ngx.log, "gateway", "the tunnel stream failed").await;
}
