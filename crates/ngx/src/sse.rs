//! SSE streaming handlers for the three admin log streams:
//! `/api/logs/stream` (access), `/api/logs/bots/stream` (bot) and
//! `/api/logs/referers/stream` (referer).
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
//! The three streams differ in exactly four places: the broadcast
//! channel they subscribe to, the ring-buffer snapshot they replay,
//! the encoder that turns an entry into a `data:` frame, and the
//! topic word that names the stream in the 401 body and in the log
//! lines. Everything else (auth, the SSE prelude, the
//! replay-then-tail loop, the lag / closed / shutdown handling and
//! the final `finish()`) is identical, so it is implemented **once**
//! in `handle_stream`; the three public handlers are thin wrappers
//! over it.
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
//! 4. Forward each entry that arrives on the broadcast channel as
//!    it arrives. On `LagError(N)`, write `: lagged N events\n\n` (an
//!    SSE comment that the browser ignores) and continue.
//! 5. Drop the receiver cleanly when the client disconnects (the
//!    `write_response_body` call returns an error → break the loop).

use std::sync::Arc;

use bytes::Bytes;
use log::{debug, info, warn};
use pangolin_core::AccessLogEntry;
use pangolin_core::BotLogEntry;
use pangolin_core::RefererLogEntry;
use pingora::apps::ReusedHttpStream;
use pingora::http::ResponseHeader;
use pingora::protocols::http::ServerSession;
use serde_json::json;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

use crate::App;

/// The one implementation behind the three admin log SSE streams.
///
/// The stream-specific bits arrive as parameters: `subscribe` picks
/// the broadcast channel, `replay` picks the ring-buffer snapshot
/// and `encode` turns one entry into a `data:` frame. `topic` names
/// the stream — it is the word in the 401 body ("Admin login
/// required to view *access logs*.") and the bracket in every log
/// line this function writes.
///
/// `T: Clone` is a hard constraint: `broadcast::Receiver::recv()`
/// hands the entry out by value. `T: Send` is one too: the public
/// handlers are awaited from `#[async_trait]`, so the whole future
/// chain (wrapper → `handle_stream` → `recv()`) must be `Send`.
///
/// The parameter list is long because the three streams pass their
/// own channel, snapshot and encoder; grouping them into a struct
/// would only move the same values one level down.
#[allow(clippy::too_many_arguments)]
async fn handle_stream<T, Sub, Rep, Enc>(
    mut session: ServerSession,
    app: Arc<App>,
    sessions: Arc<::admin::state::SessionStore>,
    cookie: Option<&str>,
    mut shutdown: pingora::server::ShutdownWatch,
    topic: &'static str,
    subscribe: Sub,
    replay: Rep,
    encode: Enc,
) -> Option<ReusedHttpStream>
where
    T: Clone + Send + 'static,
    Sub: FnOnce(&App) -> broadcast::Receiver<T> + Send,
    Rep: FnOnce(&App) -> Vec<T> + Send,
    Enc: Fn(&T) -> Bytes + Send + Sync,
{
    // 1) Auth — admin-only. Non-admin attempts get a normal 401
    //    JSON response, not an SSE frame.
    let token = cookie.and_then(::admin::state::parse_session_cookie);
    let authed = match token {
        Some(t) => sessions.validate(&t).await,
        None => false,
    };
    if !authed {
        return write_sse_unauth(session, topic).await;
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
            warn!("SSE: [{topic}] failed to build response: {e}");
            return None;
        }
    };
    let (parts, _body) = resp.into_parts();
    let header: ResponseHeader = parts.into();
    if let Err(e) = session.write_response_header(Box::new(header)).await {
        warn!("SSE: [{topic}] write_response_header failed: {e}");
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
    let mut rx = subscribe(&app);
    let snapshot = replay(&app);
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
            write_chunk(&mut session, encode(&entry)).await?;
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
            info!("SSE: [{topic}] shutdown already signalled, ending live tail");
            let _ = write_chunk(&mut session, frame_comment("shutdown")).await;
            break;
        }
        tokio::select! {
            // Bias toward shutdown so a Ctrl-C during a long
            // broadcast gap doesn't have to wait for the next
            // entry to arrive.
            biased;
            _ = shutdown.changed() => {
                info!("SSE: [{topic}] shutdown observed, ending live tail");
                let _ = write_chunk(&mut session, frame_comment("shutdown")).await;
                break;
            }
            recv = rx.recv() => match recv {
                Ok(entry) => {
                    if write_chunk(&mut session, encode(&entry))
                        .await
                        .is_none()
                    {
                        // Client disconnected.
                        debug!("SSE: [{topic}] client disconnected, stopping live tail");
                        return None;
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    // Subscriber fell behind. Surface as an SSE comment
                    // (browsers ignore these, devtools shows them).
                    // The next successful recv() will deliver the
                    // newest message the channel still has.
                    write_chunk(&mut session, frame_comment(&format!("lagged {n} events"))).await?;
                    warn!("SSE: [{topic}] subscriber lagged by {n} events");
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
            warn!("SSE: [{topic}] finish failed: {e}");
            None
        }
    }
}

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
///
/// The stream itself is implemented by `handle_stream`; this
/// wrapper only names the access-log channel, snapshot and encoder.
pub async fn handle_access_log_stream(
    session: ServerSession,
    app: Arc<App>,
    sessions: Arc<::admin::state::SessionStore>,
    cookie: Option<&str>,
    shutdown: pingora::server::ShutdownWatch,
) -> Option<ReusedHttpStream> {
    handle_stream(
        session,
        app,
        sessions,
        cookie,
        shutdown,
        "access logs",
        |app| app.access_log_tx.subscribe(),
        |app| app.recent_access_log(),
        frame_data,
    )
    .await
}

/// Handle the `/api/logs/bots/stream` SSE request.
///
/// Searchenginebots stage-2: subscribes to [`App::bot_log_tx`]
/// instead of the access-log channel. The bot side-channel is
/// populated by [`App::push_access_log`] whenever a request's
/// `User-Agent` matches a known bot (see
/// `pangolin_core::bot::verify_bot`). Real-browser / curl /
/// Postman traffic never reaches this stream.
///
/// The auth / prelude / replay / live-loop / shutdown handling is
/// shared with the other two streams — see `handle_stream`.
pub async fn handle_bot_log_stream(
    session: ServerSession,
    app: Arc<App>,
    sessions: Arc<::admin::state::SessionStore>,
    cookie: Option<&str>,
    shutdown: pingora::server::ShutdownWatch,
) -> Option<ReusedHttpStream> {
    handle_stream(
        session,
        app,
        sessions,
        cookie,
        shutdown,
        "bot logs",
        |app| app.bot_log_tx.subscribe(),
        |app| app.recent_bot_log(),
        frame_bot_data,
    )
    .await
}

/// Handle the `/api/logs/referers/stream` SSE request.
///
/// Subscribes to [`App::referer_log_tx`] instead of the access-log
/// channel. The referer side-channel is populated by
/// [`App::push_referer_log`] whenever the proxy classified a request
/// as an external document navigation (see `classify_referer` in
/// `proxy.rs`).
///
/// The auth / prelude / replay / live-loop / shutdown handling is
/// shared with the other two streams — see `handle_stream`.
pub async fn handle_referer_log_stream(
    session: ServerSession,
    app: Arc<App>,
    sessions: Arc<::admin::state::SessionStore>,
    cookie: Option<&str>,
    shutdown: pingora::server::ShutdownWatch,
) -> Option<ReusedHttpStream> {
    handle_stream(
        session,
        app,
        sessions,
        cookie,
        shutdown,
        "referer logs",
        |app| app.referer_log_tx.subscribe(),
        |app| app.recent_referer_log(),
        frame_referer_data,
    )
    .await
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

/// Build an SSE `data:` frame for one bot log entry.
///
/// Identical to [`frame_data`] but typed against [`BotLogEntry`].
/// Kept as a separate helper so the wire format is locked down
/// per-stream (a future field addition to `BotLogEntry` won't
/// accidentally leak into the access-log stream's frames).
fn frame_bot_data(entry: &BotLogEntry) -> Bytes {
    let json = serde_json::to_string(entry).unwrap_or_else(|_| "{}".to_string());
    let mut buf = Vec::with_capacity(json.len() + 8);
    buf.extend_from_slice(b"data: ");
    buf.extend_from_slice(json.as_bytes());
    buf.extend_from_slice(b"\n\n");
    Bytes::from(buf)
}

/// Build an SSE `data:` frame for one referer log entry.
///
/// Identical to [`frame_data`] but typed against [`RefererLogEntry`].
/// Kept as a separate helper so the wire format is locked down
/// per-stream (a future field addition to `RefererLogEntry` won't
/// accidentally leak into the other streams' frames).
fn frame_referer_data(entry: &RefererLogEntry) -> Bytes {
    let json = serde_json::to_string(entry).unwrap_or_else(|_| "{}".to_string());
    let mut buf = Vec::with_capacity(json.len() + 8);
    buf.extend_from_slice(b"data: ");
    buf.extend_from_slice(json.as_bytes());
    buf.extend_from_slice(b"\n\n");
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

/// Write a 401 response for an unauthenticated SSE request.
///
/// The response is **not** SSE-shaped: it carries
/// `Content-Type: application/json` and a JSON body, so the
/// browser's `EventSource` never parses it as `data:` frames and
/// errors out — it just sees a failed connection and surfaces the
/// "disconnected" state the admin UI already handles.
///
/// `topic` names the stream in the message body ("Admin login
/// required to view *bot logs*."), so the error doesn't lie to the
/// operator about which stream they tried to open.
async fn write_sse_unauth(mut session: ServerSession, topic: &str) -> Option<ReusedHttpStream> {
    let body = json!({
        "error": "unauthenticated",
        "message": format!("Admin login required to view {topic}."),
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
            warn!("SSE: [{topic}] finish failed: {e}");
            None
        }
    }
}
