//! Referer log side-channel — end-to-end tests for issue #142.
//!
//! ## What we exercise
//!
//! A real `pangolin-ngx` receives proxied requests; the referer log
//! must record **only** the ones that are external document
//! navigations. Four cases, all against the same running gateway:
//!
//! 1. **An external document navigation writes a row.** A `Referer`
//!    naming a host that belongs to no site of ours, plus
//!    `Sec-Fetch-Dest: document`, produces one JSONL row whose
//!    `referer_host` is the referrer and whose `referer` is the URL
//!    without its query string.
//!
//! 2. **An internal referral writes none.** A `Referer` naming
//!    another domain of the *current* site is internal — it must not
//!    appear.
//!
//! 3. **A subresource request writes none.** `Sec-Fetch-Dest: script`
//!    is not a document navigation, so the row is suppressed even
//!    though the request carries an external referer.
//!
//! 4. **A request without a referer writes none.**
//!
//! The rows are read back from the on-disk JSONL file, which is the
//! artifact the operator actually inspects with `jq`.
//!
//! ## Why the file and not the SSE stream
//!
//! The SSE stream is a nice-to-have; the JSONL file is the contract.
//! Reading the file also sidesteps the browser-only nature of the
//! `EventSource` consumer — the admin UI's own wiring is covered by
//! the admin crate's unit tests.
//!
//! Prerequisite: `make build` (the tests spawn `pangolin-ngx` from
//! `target/release/` or `bin/`).

use std::time::Duration;

use chrono::Utc;
use pangolin_core::db;
use pangolin_core::types::{Domain, HostMode, Site};
use rusqlite::Connection;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::timeout;

use crate::harness::{NgxProcess, init_pangolin_db, raw_request_with_headers};

// ─────────────────────────────────────────────────────────────────
// DB seed helpers
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
            challenge_kind: None,
            created_at: Utc::now(),
        },
    )
    .expect("insert domain");
}

// ─────────────────────────────────────────────────────────────────
// Mock upstream — answers 200 OK to every request.
// ─────────────────────────────────────────────────────────────────

/// Tiny HTTP backend that replies 200 OK to every request. The
/// referer log records the request before the response is read, so
/// the backend's content is irrelevant.
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
// Helpers
// ─────────────────────────────────────────────────────────────────

/// The JSONL file the running gateway writes for today.
///
/// The referer log's directory defaults to `./logs/referers` — a
/// *relative* path — and the harness spawns the binary with its CWD
/// set to the per-test tempdir, so the file lands under
/// `<work_dir>/logs/referers/`.
fn referer_jsonl_path(ngx: &NgxProcess) -> std::path::PathBuf {
    let today = Utc::now().date_naive();
    ngx.work_dir()
        .join("logs")
        .join("referers")
        .join(format!("referer-{today}.jsonl"))
}

/// Read the rows the gateway has written so far. Returns an empty
/// vec when the file doesn't exist yet.
fn read_referer_rows(ngx: &NgxProcess) -> Vec<serde_json::Value> {
    let path = referer_jsonl_path(ngx);
    let Ok(body) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect()
}

/// Poll the JSONL file until it holds at least `want` rows, or the
/// budget runs out.
///
/// The writer is a background task: `enqueue` pokes a `Notify` and
/// the actual `fs::write` happens on the writer's own task, so a
/// row is not on disk the instant the response comes back. Polling
/// (rather than a fixed sleep) keeps the test fast without being
/// flaky.
async fn wait_for_rows(ngx: &NgxProcess, want: usize) -> Vec<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let rows = read_referer_rows(ngx);
        if rows.len() >= want {
            return rows;
        }
        if tokio::time::Instant::now() >= deadline {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Give the writer a moment to flush, then assert that **no** row
/// was written for the requests just made.
///
/// The negative case needs a positive wait: the writer is
/// asynchronous, so "the file is empty right now" proves nothing.
async fn assert_no_rows(ngx: &NgxProcess) {
    tokio::time::sleep(Duration::from_millis(500)).await;
    let rows = read_referer_rows(ngx);
    assert!(
        rows.is_empty(),
        "expected no referer rows, got: {rows:#?}\nngx log:\n{}",
        ngx.log_string()
    );
}

// ─────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────

/// The core behaviour: an external document navigation is recorded;
/// an internal referral, a subresource request and a request without
/// a referer are not.
#[tokio::test]
async fn referer_log_records_only_external_document_navigations() {
    let backend = MockHttpBackend::start().await;
    let backend_addr = backend.addr().to_string();
    let ngx = NgxProcess::start({
        let backend_addr = backend_addr.clone();
        move |db_path| {
            init_pangolin_db(db_path);
            let conn = Connection::open(db_path).expect("open db");
            seed_site(&conn, "web", &format!("http://{backend_addr}"));
            seed_domain(&conn, "example.test", "web");
            // A second domain of the *same* site — a referral from it
            // is internal.
            seed_domain(&conn, "www.example.test", "web");
        }
    })
    .await;

    let addr = format!("127.0.0.1:{}", ngx.http_port);
    let host = "example.test";

    // 1) An external document navigation → a row.
    let (status, _) = raw_request_with_headers(
        &addr,
        host,
        "GET",
        "/pricing",
        b"",
        &[
            ("Referer", "https://news.example.org/article?id=7"),
            ("Sec-Fetch-Dest", "document"),
        ],
    )
    .await;
    assert_eq!(status, 200, "the proxied request must succeed");

    let rows = wait_for_rows(&ngx, 1).await;
    assert_eq!(
        rows.len(),
        1,
        "the external document navigation must produce exactly one row, got {rows:#?}"
    );
    let row = &rows[0];
    assert_eq!(row["referer_host"], "news.example.org");
    // The stored URL drops the query string.
    assert_eq!(row["referer"], "https://news.example.org/article");
    assert_eq!(row["host"], host);
    assert_eq!(row["path"], "/pricing");
    assert_eq!(row["method"], "GET");
    assert_eq!(row["status"], 200);
    // R5: the key set is exactly the documented one — no site name.
    let mut keys: Vec<&str> = row
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.as_str())
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "backend",
            "client_ip",
            "duration_ms",
            "host",
            "method",
            "path",
            "referer",
            "referer_host",
            "status",
            "timestamp",
            "ua",
        ]
    );

    // 2) An internal referral (another domain of the same site) → no
    //    new row.
    let (status, _) = raw_request_with_headers(
        &addr,
        host,
        "GET",
        "/internal",
        b"",
        &[
            ("Referer", "https://www.example.test/other"),
            ("Sec-Fetch-Dest", "document"),
        ],
    )
    .await;
    assert_eq!(status, 200);

    // 3) A subresource request carrying the same external referer →
    //    no new row.
    let (status, _) = raw_request_with_headers(
        &addr,
        host,
        "GET",
        "/app.js",
        b"",
        &[
            ("Referer", "https://news.example.org/article?id=7"),
            ("Sec-Fetch-Dest", "script"),
        ],
    )
    .await;
    assert_eq!(status, 200);

    // 4) A request without a referer → no new row.
    let (status, _) = raw_request_with_headers(
        &addr,
        host,
        "GET",
        "/direct",
        b"",
        &[("Sec-Fetch-Dest", "document")],
    )
    .await;
    assert_eq!(status, 200);

    // Give the writer time to flush anything it might still be
    // holding, then assert that only the one row survives.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let rows = read_referer_rows(&ngx);
    assert_eq!(
        rows.len(),
        1,
        "only the external document navigation may be recorded, got {rows:#?}\nngx log:\n{}",
        ngx.log_string()
    );
    assert_eq!(rows[0]["referer_host"], "news.example.org");
    assert_eq!(rows[0]["path"], "/pricing");
}

/// The `Accept` fallback: a client that omits `Sec-Fetch-Dest` is
/// eligible only when its `Accept` header contains `text/html`.
#[tokio::test]
async fn referer_log_falls_back_to_the_accept_header() {
    let backend = MockHttpBackend::start().await;
    let backend_addr = backend.addr().to_string();
    let ngx = NgxProcess::start({
        let backend_addr = backend_addr.clone();
        move |db_path| {
            init_pangolin_db(db_path);
            let conn = Connection::open(db_path).expect("open db");
            seed_site(&conn, "web", &format!("http://{backend_addr}"));
            seed_domain(&conn, "example.test", "web");
        }
    })
    .await;

    let addr = format!("127.0.0.1:{}", ngx.http_port);
    let host = "example.test";

    // `Accept: text/html` and no `Sec-Fetch-Dest` → eligible.
    let (status, _) = raw_request_with_headers(
        &addr,
        host,
        "GET",
        "/html",
        b"",
        &[
            ("Referer", "https://news.example.org/a"),
            ("Accept", "text/html,application/xhtml+xml"),
        ],
    )
    .await;
    assert_eq!(status, 200);

    // `Accept: application/json` and no `Sec-Fetch-Dest` → not
    // eligible.
    let (status, _) = raw_request_with_headers(
        &addr,
        host,
        "GET",
        "/json",
        b"",
        &[
            ("Referer", "https://news.example.org/b"),
            ("Accept", "application/json"),
        ],
    )
    .await;
    assert_eq!(status, 200);

    let rows = wait_for_rows(&ngx, 1).await;
    assert_eq!(
        rows.len(),
        1,
        "only the text/html navigation may be recorded, got {rows:#?}"
    );
    assert_eq!(rows[0]["path"], "/html");
}

/// A referer that yields no host (`about:blank`) is not recorded.
#[tokio::test]
async fn referer_log_ignores_referers_without_a_host() {
    let backend = MockHttpBackend::start().await;
    let backend_addr = backend.addr().to_string();
    let ngx = NgxProcess::start({
        let backend_addr = backend_addr.clone();
        move |db_path| {
            init_pangolin_db(db_path);
            let conn = Connection::open(db_path).expect("open db");
            seed_site(&conn, "web", &format!("http://{backend_addr}"));
            seed_domain(&conn, "example.test", "web");
        }
    })
    .await;

    let addr = format!("127.0.0.1:{}", ngx.http_port);
    for referer in ["about:blank", "/relative/path"] {
        let (status, _) = raw_request_with_headers(
            &addr,
            "example.test",
            "GET",
            "/x",
            b"",
            &[("Referer", referer), ("Sec-Fetch-Dest", "document")],
        )
        .await;
        assert_eq!(status, 200);
    }

    assert_no_rows(&ngx).await;
}

/// The referer log's admin pages are reachable by an authenticated
/// admin and render the site selector.
#[tokio::test]
async fn referer_admin_pages_render_for_an_authenticated_admin() {
    let backend = MockHttpBackend::start().await;
    let backend_addr = backend.addr().to_string();
    let ngx = NgxProcess::start({
        let backend_addr = backend_addr.clone();
        move |db_path| {
            init_pangolin_db(db_path);
            let conn = Connection::open(db_path).expect("open db");
            seed_site(&conn, "web", &format!("http://{backend_addr}"));
            seed_domain(&conn, "example.test", "web");
        }
    })
    .await;

    let client = crate::admin_harness::AdminClient::new(&ngx);
    client.login("admin", "admin").await.expect("login");

    for path in [
        "/logs/referers",
        "/logs/referers/history",
        "/logs/referers/stats",
    ] {
        let resp = client.get(path).await.expect("admin page");
        assert_eq!(resp.status().as_u16(), 200, "{path} must render");
        let body = resp.text().await.expect("body");
        assert!(
            body.contains("Referer Logs"),
            "{path} is missing the page title: {body}"
        );
        assert!(
            body.contains(">Referers</a>"),
            "{path} is missing the nav entry: {body}"
        );
    }
}

/// The referer pages redirect an anonymous request to `/login`, like
/// every other admin page (R9).
#[tokio::test]
async fn referer_admin_pages_require_auth() {
    let backend = MockHttpBackend::start().await;
    let backend_addr = backend.addr().to_string();
    let ngx = NgxProcess::start({
        let backend_addr = backend_addr.clone();
        move |db_path| {
            init_pangolin_db(db_path);
            let conn = Connection::open(db_path).expect("open db");
            seed_site(&conn, "web", &format!("http://{backend_addr}"));
            seed_domain(&conn, "example.test", "web");
        }
    })
    .await;

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("reqwest client");

    for path in [
        "/logs/referers",
        "/logs/referers/history",
        "/logs/referers/stats",
    ] {
        let resp = client
            .get(format!("http://127.0.0.1:{}{}", ngx.admin_port, path))
            .send()
            .await
            .expect("GET admin page");
        assert_eq!(
            resp.status().as_u16(),
            302,
            "an unauthenticated {path} must redirect to /login"
        );
        let location = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            location.starts_with("/login"),
            "{path} must redirect to /login, got {location:?}"
        );
    }
}

/// The referer SSE endpoint is admin-only: an anonymous GET must get
/// a 401 with a plain error body, **not** an SSE frame (R12).
///
/// An SSE-shaped 401 would make the browser's `EventSource` retry
/// forever — the endpoint writes a normal (non-`text/event-stream`)
/// error body for exactly this reason.
#[tokio::test]
async fn referer_sse_stream_requires_auth() {
    let backend = MockHttpBackend::start().await;
    let backend_addr = backend.addr().to_string();
    let ngx = NgxProcess::start({
        let backend_addr = backend_addr.clone();
        move |db_path| {
            init_pangolin_db(db_path);
            let conn = Connection::open(db_path).expect("open db");
            seed_site(&conn, "web", &format!("http://{backend_addr}"));
            seed_domain(&conn, "example.test", "web");
        }
    })
    .await;

    let addr = format!("127.0.0.1:{}", ngx.admin_port);
    let mut stream = timeout(
        Duration::from_secs(5),
        tokio::net::TcpStream::connect(&addr),
    )
    .await
    .expect("connect")
    .expect("connect");
    let req = "GET /api/logs/referers/stream HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: text/event-stream\r\nConnection: close\r\n\r\n";
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
    assert!(
        !text.contains("text/event-stream"),
        "the 401 must not be an SSE response: {text:?}"
    );
    assert!(
        !text.contains("data:"),
        "the 401 must not carry an SSE frame: {text:?}"
    );
}
