//! Bot log side-channel — end-to-end tests for the
//! `/api/logs/bots/stream` SSE endpoint.
//!
//! ## What we exercise
//!
//! 1. **Unauthenticated access to `/api/logs/bots/stream` returns
//!    401** — the bot stream is admin-only, and the 401 must be a
//!    plain JSON body rather than an SSE-shaped one. The browser's
//!    `EventSource` would parse an SSE-shaped error as a `data:`
//!    frame and retry forever.
//!
//! 2. **Replay then live frames** — the admin opens the stream, gets
//!    the ring-buffer snapshot (oldest-first) and then sees new
//!    entries arrive in real time. Both halves are asserted, so a
//!    change that breaks only one of them fails here.
//!
//! 3. **JSON shape** — every `data:` frame is one `BotLogEntry`
//!    serialised with serde_json. We assert the bot-identification
//!    fields (`bot_name`, `bot_vendor`, `bot_category`) next to the
//!    shared request fields, so a change to the knownbots adapter
//!    cannot silently change the wire format.
//!
//! ## Making the client a *verified* bot
//!
//! The bot side-channel only records requests the knownbots verifier
//! recognises — a spoofed `User-Agent` alone is dropped (the
//! verifier is fail-closed). The e2e therefore seeds a registry
//! override (`harness::seed_googlebot_registry`) that maps the
//! loopback address to `googlebot` before the gateway boots.
//!
//! Prerequisite: `make build` (the tests spawn `pangolin-ngx` from
//! `target/release/` or `bin/`).

use std::time::Duration;

use chrono::Utc;
use pangolin_core::db;
use pangolin_core::types::{Domain, HostMode, Site};
use rusqlite::Connection;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::harness::{
    InspectingBackend, NgxProcess, admin_session_cookie, init_pangolin_db,
    raw_request_with_headers, read_sse_data_frames, seed_googlebot_registry,
};

/// The user agent the seeded registry matches. The marker is the
/// word `Googlebot` — the verifier matches it on a word boundary.
const GOOGLEBOT_UA: &str =
    "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)";

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
// Tests
// ─────────────────────────────────────────────────────────────────

/// The bot SSE endpoint is admin-only: an anonymous GET must get a
/// 401 with a plain error body, **not** an SSE frame.
///
/// An SSE-shaped 401 would make the browser's `EventSource` retry
/// forever — the endpoint writes a normal (non-`text/event-stream`)
/// error body for exactly this reason.
#[tokio::test]
async fn bot_log_sse_requires_auth() {
    let backend = InspectingBackend::start().await;
    let ngx = NgxProcess::start({
        let addr = backend.addr().to_string();
        move |db_path| {
            init_pangolin_db(db_path);
            let conn = Connection::open(db_path).expect("open db");
            seed_site(&conn, "bot-site", &format!("http://{addr}"));
            seed_domain(&conn, "bot.test", "bot-site");
        }
    })
    .await;

    let addr = format!("127.0.0.1:{}", ngx.admin_port);
    let mut stream = timeout(Duration::from_secs(5), TcpStream::connect(&addr))
        .await
        .expect("connect")
        .expect("connect");
    let req = "GET /api/logs/bots/stream HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: text/event-stream\r\nConnection: close\r\n\r\n";
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

/// The bot stream replays the ring buffer and then tails it live.
///
/// We assert:
///   1. The request sent *before* the SSE connection opened is
///      replayed as a `data:` frame (the ring-buffer snapshot).
///   2. The request sent *after* the connection opened arrives on
///      the live tail.
///   3. Both frames carry the bot identification the seeded registry
///      produced — `bot_name`, `bot_vendor` and `bot_category`.
///
/// The bot entry only exists because the seeded registry makes the
/// loopback address a *verified* googlebot; without it the verifier
/// drops the request and the stream stays empty.
#[tokio::test]
async fn bot_log_sse_replays_then_streams_live() {
    let backend = InspectingBackend::start().await;
    let ngx = NgxProcess::start({
        let addr = backend.addr().to_string();
        move |db_path| {
            init_pangolin_db(db_path);
            let conn = Connection::open(db_path).expect("open db");
            seed_site(&conn, "bot-site", &format!("http://{addr}"));
            seed_domain(&conn, "bot.test", "bot-site");
            // The registry the verifier reads lives in the cache
            // root, which defaults to `./logs/bots/cache` relative
            // to the process's working directory — the tempdir.
            let cache_root = db_path
                .parent()
                .expect("the db lives in the tempdir")
                .join("logs/bots/cache");
            seed_googlebot_registry(&cache_root);
        }
    })
    .await;

    // 0) Authenticate. The SSE endpoint is admin-only — covered by
    //    `bot_log_sse_requires_auth`.
    let session_cookie = admin_session_cookie(&ngx).await;

    // 1) A verified-bot request BEFORE the SSE connection opens. The
    //    proxy's response_filter pushes a BotLogEntry to the bot
    //    ring buffer; we expect it to be replayed on connect.
    let http_addr = format!("127.0.0.1:{}", ngx.http_port);
    let (status_before, _) = raw_request_with_headers(
        &http_addr,
        "bot.test",
        "GET",
        "/api/before",
        b"",
        &[("User-Agent", GOOGLEBOT_UA)],
    )
    .await;
    assert_eq!(status_before, 200, "pre-SSE request must succeed");

    // 2) Open the SSE connection. The pre-SSE entry must be
    //    replayed as a `data:` frame.
    let sse_addr = format!("127.0.0.1:{}", ngx.admin_port);
    let within = Duration::from_secs(3);
    let sse_cookie = session_cookie.clone();
    let sse_task = tokio::spawn(async move {
        read_sse_data_frames(
            &sse_addr,
            "/api/logs/bots/stream",
            within,
            Some(&sse_cookie),
        )
        .await
    });

    // Give the SSE handshake a moment to complete and the replay
    // frames to land before the live request goes out.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 3) A second verified-bot request AFTER the connection — the
    //    "real-time push" half.
    let (status_after, _) = raw_request_with_headers(
        &http_addr,
        "bot.test",
        "GET",
        "/api/after",
        b"",
        &[("User-Agent", GOOGLEBOT_UA)],
    )
    .await;
    assert_eq!(status_after, 200, "post-SSE request must succeed");

    let frames = sse_task.await.expect("sse task join").expect("sse frames");
    assert!(
        !frames.is_empty(),
        "expected at least one SSE data frame, got 0. \
         ngx log:\n{}",
        ngx.log_string()
    );

    let entry_for = |path: &str| -> serde_json::Value {
        frames
            .iter()
            .map(|s| serde_json::from_str::<serde_json::Value>(s))
            .find_map(|r| {
                r.ok().and_then(|j| {
                    let matches = j.get("path").and_then(|p| p.as_str()) == Some(path);
                    if matches { Some(j) } else { None }
                })
            })
            .unwrap_or_else(|| {
                panic!(
                    "no SSE frame for path={path}. frames={frames:?}\nngx log:\n{}",
                    ngx.log_string()
                )
            })
    };

    let pre = entry_for("/api/before");
    let post = entry_for("/api/after");

    for entry in [&pre, &post] {
        // The bot identification the seeded registry produced.
        assert_eq!(entry["bot_name"], "googlebot");
        assert_eq!(entry["bot_vendor"], "Google");
        assert_eq!(entry["bot_category"], "search_engine");
        // The shared request fields.
        assert_eq!(entry["method"], "GET");
        assert_eq!(entry["host"], "bot.test");
        assert_eq!(entry["status"], 200);
        assert!(
            entry["duration_ms"].as_u64().is_some(),
            "duration_ms must be a number: {entry}"
        );
        assert_eq!(entry["backend"], format!("direct:{}", backend.addr()));
    }
}
