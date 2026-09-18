//! SSE streaming handler for `/api/logs/stream` (issue #73).
//!
//! Lives in the `ngx` crate (not `admin`) because:
//!   - The SSE writer owns the `ServerSession` and calls
//!     `write_response_body(.., false)` repeatedly — only pingora's
//!     `HttpServerApp` path allows this (admin's `ServeHttp`-based
//!     pipeline materialises the whole body before returning).
//!   - The admin route layer still owns the page handler (no
//!     streaming involved); this module only owns the streaming
//!     chunk-writer.
//!
//! ## Design
//!
//! 1. Authenticate: parse the session cookie and call
//!    `SessionStore::validate`. 401 if invalid (admin endpoints are
//!    not public). CSRF is not enforced — SSE is a read-only stream
//!    and the `EventSource` browser API can't send custom headers /
//!    POST body to set a CSRF token anyway.
//! 2. Write the SSE prelude:
//!      - Status 200
//!      - `Content-Type: text/event-stream`
//!      - `Cache-Control: no-cache`
//!      - `X-Accel-Buffering: no` (disable nginx-level buffering)
//! 3. Drain the ring buffer snapshot as `data: <json>\n\n` frames
//!    (oldest first).
//! 4. Subscribe to `App::access_log_tx` and forward each entry as
//!    it arrives. On `LagError(N)`, write
//!    `: lagged N events\n\n` (an SSE comment that the browser
//!    ignores) and continue.
//! 5. Drop the receiver cleanly when the client disconnects (the
//!    `write_response_body` call returns an error → break the loop).

use std::sync::Arc;

use bytes::Bytes;
use log::{debug, info, warn};
use pangolin_core::AccessLogEntry;
use pingora::apps::ReusedHttpStream;
use pingora::http::ResponseHeader;
use pingora::protocols::http::ServerSession;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use crate::App;

/// Handle the `/api/logs/stream` SSE request.
///
/// Returns `Some(ReusedHttpStream)` on a clean shutdown (the
/// connection may be reused); `None` on error (the caller logs).
///
/// `shutdown` is an owned clone of pingora's
/// `ShutdownWatch` — a `tokio::sync::watch::Receiver<bool>`
/// the runtime flips on SIGINT/SIGTERM. We `select!` on it
/// inside the live broadcast loop so a Ctrl-C doesn't have to
/// wait for `graceful_shutdown_timeout_seconds` to expire
/// before the SSE handler yields. Without this, an idle SSE
/// client (no new access-log entries) would hold the runtime
/// open for the full 5 s pingora-configured drain window —
/// visible in the log as
/// `Waiting for service runtime pangolin-http to exit` — and
/// the operator-facing shutdown latency would be `grace_period
/// + graceful_shutdown_timeout` instead of `grace_period`
///   alone.
pub async fn handle_access_log_stream(
    mut session: ServerSession,
    app: Arc<App>,
    sessions: Arc<::admin::state::SessionStore>,
    cookie: Option<&str>,
    mut shutdown: pingora::server::ShutdownWatch,
) -> Option<ReusedHttpStream> {
    // 1) Auth — admin-only. Non-admin attempts get a normal 401
    //    HTML response, not an SSE frame.
    let token = cookie.and_then(::admin::state::parse_session_cookie);
    let authed = match token {
        Some(t) => sessions.validate(&t).await,
        None => false,
    };
    if !authed {
        return write_sse_unauth(session).await;
    }

    // 2) SSE prelude.
    let status = http::StatusCode::OK;
    let builder = http::Response::builder()
        .status(status)
        // text/event-stream is the SSE content type per the WHATWG
        // server-sent events spec. charset=utf-8 is implicit but
        // some intermediaries (CDNs, devtools) display it.
        .header("Content-Type", "text/event-stream; charset=utf-8")
        .header("Cache-Control", "no-cache")
        // Disable any nginx / CDN / reverse-proxy buffering so the
        // browser sees frames as soon as we send them. The default
        // for `text/event-stream` is *not* buffered, but `no-cache`
        // + `X-Accel-Buffering: no` together is the most
        // interoperable hint.
        .header("X-Accel-Buffering", "no")
        // Connection: keep-alive is the default for HTTP/1.1 but
        // we set it explicitly because some intermediaries drop the
        // connection otherwise.
        .header("Connection", "keep-alive");
    // We deliberately do NOT set `Transfer-Encoding: chunked` here.
    // Pingora sets it automatically when the body is streamed via
    // `write_response_body(.., false)`. On HTTP/2 the header is
    // forbidden entirely (HTTP/2 frames its own body), so setting
    // it explicitly risks an "illegal connection-specific header"
    // rejection from pingora's h2 layer.

    let resp: http::Response<()> = match builder.body(()) {
        Ok(r) => r,
        Err(e) => {
            warn!("SSE: failed to build response: {e}");
            return None;
        }
    };
    let (parts, _body) = resp.into_parts();
    let header: ResponseHeader = parts.into();
    if let Err(e) = session.write_response_header(Box::new(header)).await {
        warn!("SSE: write_response_header failed: {e}");
        return None;
    }

    // 3) Subscribe FIRST, then snapshot. Subscribing first guarantees
    //    we capture every entry pushed from this moment forward; the
    //    snapshot then covers everything pushed before. The window
    //    between the two operations can produce at most one DUPLICATE
    //    per entry (snapshot saw it AND the channel later delivers
    //    it), never a gap — which is the correct trade-off for an
    //    operator viewer ("show me everything, possibly with brief
    //    duplicates" beats "show me almost everything").
    //
    //    The previous order (snapshot → subscribe) had a TOCTOU race:
    //    entries pushed between the two calls were in neither the
    //    snapshot nor the live stream and were silently lost.
    let mut rx = app.access_log_tx.subscribe();
    let snapshot = app.recent_access_log();
    if snapshot.is_empty() {
        // Tell the client there is no replay. SSE comments are
        // ignored by `EventSource` but visible in devtools, which
        // is exactly the right surface for an operator debugging a
        // missing-events report.
        write_chunk(&mut session, frame_comment("replay: empty")).await?;
    } else {
        // Capture the length BEFORE the `for` loop consumes the
        // Vec — moving `snapshot` into the iterator drops the
        // value before we can read `.len()` again.
        let snapshot_len = snapshot.len();
        for entry in snapshot {
            write_chunk(&mut session, frame_data(&entry)).await?;
        }
        write_chunk(
            &mut session,
            frame_comment(&format!("replay: done ({})", snapshot_len)),
        )
        .await?;
    }

    // 4) Live broadcast loop. Race the broadcast receiver
    //    against pingora's ShutdownWatch so a Ctrl-C during
    //    a long-lived SSE connection drops the connection
    //    promptly instead of waiting for the full
    //    `graceful_shutdown_timeout_seconds` window.
    //
    // `ShutdownWatch` is a `tokio::sync::watch::Receiver<bool>`
    // (pingora-core/src/server/mod.rs) — its `changed()` future
    // resolves the moment the server flips its shutdown flag.
    // Calling `borrow()` first is the cheap fast path: if the
    // flag is *already* set (e.g. the loop is entered after a
    // shutdown that arrived between subscribe() and the first
    // poll), we break without consuming a broadcast slot. The
    // `select!` below would catch it too, but the early
    // check keeps the log line ("shutdown observed") distinct
    // from a poll that races on shutdown mid-loop.
    loop {
        if *shutdown.borrow() {
            info!("SSE: shutdown already signalled, ending live tail");
            let _ = write_chunk(&mut session, frame_comment("shutdown")).await;
            break;
        }
        tokio::select! {
            // Bias toward shutdown so a Ctrl-C during a long
            // broadcast gap doesn't have to wait for the next
            // entry to arrive.
            biased;
            _ = shutdown.changed() => {
                info!("SSE: shutdown observed, ending live tail");
                let _ = write_chunk(&mut session, frame_comment("shutdown")).await;
                break;
            }
            recv = rx.recv() => match recv {
                Ok(entry) => {
                    if write_chunk(&mut session, frame_data(&entry))
                        .await
                        .is_none()
                    {
                        // Client disconnected.
                        debug!("SSE: client disconnected, stopping live tail");
                        return None;
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    // Subscriber fell behind. Surface as an SSE comment
                    // (browsers ignore these, devtools shows them).
                    // The next successful recv() will deliver the
                    // newest message the channel still has.
                    write_chunk(&mut session, frame_comment(&format!("lagged {} events", n))).await?;
                    warn!("SSE: subscriber lagged by {n} events");
                }
                Err(RecvError::Closed) => {
                    // Broadcast channel closed (app shutdown). Send a
                    // terminal comment and end the stream cleanly.
                    let _ = write_chunk(&mut session, frame_comment("closed")).await;
                    break;
                }
            }
        }
    }

    // 5) Final chunk (end=true) and return the connection.
    let settings = pingora::apps::HttpPersistentSettings::for_session(&session);
    match session.finish().await {
        Ok(c) => c.map(|s| ReusedHttpStream::from_reusable_stream(s, settings)),
        Err(e) => {
            warn!("SSE: finish failed: {e}");
            None
        }
    }
}

/// Build an SSE `data:` frame for one access log entry.
///
/// Returns owned `Bytes` (not `Vec<u8>`) so the caller can hand it
/// straight to `session.write_response_body` without an extra copy.
/// The JSON is serialised inline (no extra newline escapes needed
/// because serde_json does not emit raw newlines inside a string
/// — only escaped `\n` characters, which are valid in an SSE
/// `data:` frame).
fn frame_data(entry: &AccessLogEntry) -> Bytes {
    let json = serde_json::to_string(entry).unwrap_or_else(|_| "{}".to_string());
    let mut buf = Vec::with_capacity(json.len() + 8);
    buf.extend_from_slice(b"data: ");
    buf.extend_from_slice(json.as_bytes());
    buf.extend_from_slice(b"\n\n");
    // `Bytes::from(Vec)` transfers ownership of the heap allocation
    // — no copy. The previous `Vec<u8>` + `Bytes::copy_from_slice`
    // in `write_chunk` allocated twice per frame per subscriber.
    Bytes::from(buf)
}

/// Build an SSE comment frame. Comments start with `:` and are
/// ignored by the browser's `EventSource` handler. Useful for
/// operator-visible diagnostics (replay size, lagged counts, etc.)
/// without polluting the data stream.
fn frame_comment(msg: &str) -> Bytes {
    let mut buf = Vec::with_capacity(msg.len() + 4);
    buf.extend_from_slice(b": ");
    buf.extend_from_slice(msg.as_bytes());
    buf.extend_from_slice(b"\n\n");
    Bytes::from(buf)
}

/// Write one SSE chunk to the wire. Returns `None` if the write
/// failed (client disconnect / network error); the caller should
/// treat that as "stream done". The underlying `io::Error` is
/// logged at `debug!` level so a production debug session can see
/// the actual cause (EPIPE / ECONNRESET / buffer full) — losing
/// the error silently was a real maintenance hazard for SSE
/// connection-drop investigations.
async fn write_chunk(session: &mut ServerSession, bytes: Bytes) -> Option<()> {
    match session.write_response_body(bytes, false).await {
        Ok(()) => Some(()),
        Err(e) => {
            debug!("SSE: write_response_body failed: {e}");
            None
        }
    }
}

/// Write a 401 HTML response for unauthenticated SSE requests.
///
/// We deliberately do **not** write an SSE-shaped 401 — the
/// browser's `EventSource` API silently retries on opaque errors,
/// and a JSON-shaped error would still go through the SSE parser
/// (which then tries to parse our 401 HTML as `data:` frames and
/// errors out). A plain HTML 401 is the cleanest "you must log in"
/// signal.
async fn write_sse_unauth(mut session: ServerSession) -> Option<ReusedHttpStream> {
    let body = json!({
        "error": "unauthenticated",
        "message": "Admin login required to view access logs.",
    })
    .to_string();
    let resp: http::Response<()> = http::Response::builder()
        .status(http::StatusCode::UNAUTHORIZED)
        .header("Content-Type", "application/json")
        .header("WWW-Authenticate", "Cookie")
        .body(())
        .ok()?;
    let (parts, _) = resp.into_parts();
    let header: ResponseHeader = parts.into();
    session.write_response_header(Box::new(header)).await.ok()?;
    session
        .write_response_body(Bytes::from(body), true)
        .await
        .ok()?;
    let settings = pingora::apps::HttpPersistentSettings::for_session(&session);
    match session.finish().await {
        Ok(c) => c.map(|s| ReusedHttpStream::from_reusable_stream(s, settings)),
        Err(e) => {
            warn!("SSE: finish failed: {e}");
            None
        }
    }
}
