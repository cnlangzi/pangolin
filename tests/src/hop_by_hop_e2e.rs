//! Hop-by-hop header handling — e2e against the real gateway.
//!
//! pingora 0.9 (issue #189) removed two kinds of header from the
//! **upstream** request before it is written to the backend:
//!
//! - the standard hop-by-hop headers (`Connection`, `Keep-Alive`,
//!   `Transfer-Encoding`, `Upgrade`, `TE`, `Trailer`, the
//!   `Proxy-*` family), and
//! - the headers the request's own `Connection` header **nominates**
//!   (`Connection: X-Probe` nominates `X-Probe`), which RFC 9110
//!   §7.6.1 requires to be removed before forwarding.
//!
//! The second half is new for this repository. Its own
//! `strip_hop_by_hop_headers` (`crates/pangolin-core/src/proxy.rs`)
//! never covered the nominated headers, and the direct proxy path
//! deliberately does **not** strip anything itself — it relies on
//! pingora for hop-by-hop handling (see
//! `apply_proxy_policy_without_hop_by_hop_stripping`).
//!
//! Before pingora 0.9 the nomination was not honoured at all: the
//! backend received both `Connection: X-Probe` and `X-Probe: 1`.
//! This test is the regression guard for that: it fails against a
//! build that forwards the nominated header, and it also pins that
//! the gateway's **own** headers still reach the backend — a proxy
//! that strips everything would pass a naive "the header is gone"
//! assertion.
//!
//! The test drives the real `pangolin-ngx` binary (through
//! `harness::NgxProcess`), because the behaviour under test lives in
//! pingora's proxy layer, not in this repository's code.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use pangolin_core::db;
use pangolin_core::types::{Domain, HostMode, Site};
use rusqlite::Connection;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time::timeout;

use crate::harness::{NgxProcess, init_pangolin_db};

// ---------------------------------------------------------------------------
// A raw request that reads the response head only.
// ---------------------------------------------------------------------------

/// Send a raw HTTP/1.1 request and read the **response head only**.
///
/// The shared `harness::raw_request*` helpers read to EOF.  That
/// never happens for a request whose `Connection` header does not ask
/// for a close — the gateway keeps the connection alive and the
/// helper sits there until its 5 s timeout.  This test is about the
/// headers the **backend** received, not about the connection's
/// lifetime, so it stops at the end of the response head.
async fn raw_request_head(
    addr: &str,
    host: &str,
    path: &str,
    extra_headers: &[(&str, &str)],
) -> u16 {
    let mut stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(addr))
        .await
        .expect("connect to the gateway (5s timeout)")
        .expect("connect to the gateway");

    let mut req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n");
    for (name, value) in extra_headers {
        req.push_str(&format!("{name}: {value}\r\n"));
    }
    req.push_str("\r\n");
    stream
        .write_all(req.as_bytes())
        .await
        .expect("write request head");

    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut tmp))
            .await
            .expect("read response head (5s timeout)")
            .expect("read response head");
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }

    String::from_utf8_lossy(&buf)
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Mock backend that records the request head it received.
// ---------------------------------------------------------------------------

/// One request the backend received, parsed into its header lines.
///
/// The names are lowercased: HTTP header names are case-insensitive,
/// and the assertion is about the *name*, not about the spelling the
/// gateway happened to use.
#[derive(Clone, Debug)]
struct RecordedRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
}

impl RecordedRequest {
    /// The value of the first header called `name`, if any.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Whether a header called `name` was received at all.
    fn has_header(&self, name: &str) -> bool {
        self.header(name).is_some()
    }
}

/// A backend that answers `200 OK` to everything and remembers the
/// request head of every request it received.
struct RecordingBackend {
    addr: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl RecordingBackend {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_for_spawn = requests.clone();

        let handle = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let reqs = requests_for_spawn.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::with_capacity(4096);
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
                            Ok(Err(_)) | Err(_) => break,
                        }
                    }
                    if let Some(req) = parse_request_head(&buf) {
                        reqs.lock().await.push(req);
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
            requests,
            handle,
        }
    }

    fn addr(&self) -> &str {
        &self.addr
    }

    /// The `host:port` the backend listens on — the value the
    /// gateway has to put into the `Host` header when the site's
    /// `host_mode` is `Backend`.
    fn authority(&self) -> String {
        self.addr.clone()
    }

    async fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().await.clone()
    }
}

impl Drop for RecordingBackend {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Parse the head of an HTTP/1.1 request into its method, path and
/// header lines.
fn parse_request_head(raw: &[u8]) -> Option<RecordedRequest> {
    let text = String::from_utf8_lossy(raw);
    let mut lines = text.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }

    Some(RecordedRequest {
        method,
        path,
        headers,
    })
}

// ---------------------------------------------------------------------------
// DB seed helpers
// ---------------------------------------------------------------------------

fn seed_site(conn: &Connection, name: &str, backend: &str, host_mode: HostMode) {
    let now = Utc::now();
    db::upsert_site(
        conn,
        &Site {
            name: name.into(),
            backend: backend.into(),
            enabled: true,
            host_mode,
            host_custom: None,
            created_at: now,
            updated_at: now,
            domain_count: 0,
        },
    )
    .expect("insert site");
}

fn seed_domain(conn: &Connection, domain: &str, site_name: &str) {
    let now = Utc::now();
    db::upsert_domain(
        conn,
        &Domain {
            domain: domain.into(),
            site_name: site_name.into(),
            enabled: true,
            auto_issue: false,
            dns_provider: None,
            challenge_kind: None,
            created_at: now,
        },
    )
    .expect("insert domain");
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

/// A header the request's own `Connection` header nominates must not
/// reach the backend — and neither must the `Connection` header
/// itself.
///
/// RFC 9110 §7.6.1: "A proxy or gateway MUST remove a
/// connection-specific header field before forwarding the message,
/// unless the field is listed in the Connection header field's
/// connection-options." The nominated field (`X-Probe`) is
/// connection-specific by definition, and so is `Connection` itself.
///
/// The second half of the assertion is what keeps the test honest:
/// a gateway that dropped the whole request, or that stripped every
/// header, would also make `X-Probe` disappear. So the test also
/// pins the two headers the gateway is *supposed* to add — the
/// `X-Forwarded-For` hop path and the `Host` rewrite for
/// `host_mode = Backend`.
#[tokio::test]
async fn connection_nominated_headers_are_not_forwarded() {
    let backend = RecordingBackend::start().await;

    let ngx = NgxProcess::start(|db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).unwrap();
        seed_site(
            &conn,
            "hop",
            &format!("http://{}", backend.addr()),
            HostMode::Backend,
        );
        seed_domain(&conn, "hop.test", "hop");
    })
    .await;

    let status = raw_request_head(
        &format!("127.0.0.1:{}", ngx.http_port),
        "hop.test",
        "/probe",
        &[("Connection", "X-Probe"), ("X-Probe", "1")],
    )
    .await;
    assert_eq!(status, 200, "the proxied request must succeed");

    let requests = backend.requests().await;
    assert_eq!(
        requests.len(),
        1,
        "the backend must have received exactly one request"
    );
    let received = &requests[0];

    assert_eq!(received.method, "GET");
    assert_eq!(received.path, "/probe");

    // The nominated header must not have been forwarded …
    assert!(
        !received.has_header("x-probe"),
        "the header nominated by `Connection: X-Probe` must not reach the \
         backend (RFC 9110 §7.6.1); the backend received: {:?}",
        received.headers
    );
    // … and neither must the `Connection` header itself.
    assert!(
        !received.has_header("connection"),
        "`Connection` is a hop-by-hop header and must not reach the \
         backend; the backend received: {:?}",
        received.headers
    );

    // The gateway's own headers still have to be there — otherwise
    // the two assertions above would pass for the wrong reason.
    assert!(
        received.has_header("x-forwarded-for"),
        "the gateway must still add `X-Forwarded-For`; the backend received: {:?}",
        received.headers
    );
    assert_eq!(
        received.header("host"),
        Some(backend.authority().as_str()),
        "the gateway must still rewrite `Host` to the backend's authority \
         (host_mode = Backend); the backend received: {:?}",
        received.headers
    );
}
