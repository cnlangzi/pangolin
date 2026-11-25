//! SSE streaming handlers for the three admin log streams:
//! `/api/logs/stream` (access), `/api/logs/bots/stream` (bot) and
//! `/api/logs/referers/stream` (referer).
//!
//! Lives in the `ngx` crate (not `admin`) because:
//!   - The SSE writer owns the transport's response body and has to
//!     push frames into it as they arrive — the admin crate's
//!     `handle()` returns a complete `http::Response`, so it cannot
//!     express a stream that never ends.
//!   - The admin route layer still owns the page handler (no
//!     streaming involved); this module only owns the streaming
//!     body.
//!
//! ## Design
//!
//! The three streams differ in exactly four places: the broadcast
//! channel they subscribe to, the ring-buffer snapshot they replay,
//! the encoder that turns an entry into a `data:` frame, and the
//! topic word that names the stream in the 401 body and in the log
//! lines. Everything else (auth, the SSE prelude, the
//! replay-then-tail loop, the lag / closed / shutdown handling and
//! the end of the stream) is identical, so it is implemented **once**
//! in `handle_stream`; the three public handlers are thin wrappers
//! over it.
//!
//! 1. Authenticate: parse the session cookie and call
//!    `SessionStore::validate`. 401 if invalid (admin endpoints are
//!    not public). CSRF is not enforced — SSE is a read-only stream
//!    and the `EventSource` browser API can't send custom headers /
//!    POST body to set a CSRF token anyway.
//! 2. The response carries the SSE prelude:
//!      - Status 200
//!      - `Content-Type: text/event-stream`
//!      - `Cache-Control: no-cache`
//!      - `X-Accel-Buffering: no` (disable nginx-level buffering)
//! 3. The body replays the ring buffer snapshot as `data: <json>\n\n`
//!    frames (oldest first).
//! 4. It then forwards each entry that arrives on the broadcast
//!    channel as it arrives. On `LagError(N)`, it yields
//!    `: lagged N events\n\n` (an SSE comment that the browser
//!    ignores) and continues.
//! 5. The body ends when the shutdown token is cancelled, when the
//!    channel closes, or when the client goes away — the last one
//!    because the body is dropped with the connection.
//!
//! ## Why the body is pulled, not pushed
//!
//! The frames are produced **on demand**: the response body is a
//! stream that waits for the next entry when hyper asks for one.
//! The alternative — a task that pushes the frames into a channel —
//! leaves that task behind when the client disconnects in the middle
//! of a quiet period: it would sit in `rx.recv()` until the next log
//! entry arrived. A pulled body has no task to leave behind: the
//! broadcast receiver goes away with the body.

use std::collections::VecDeque;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::Frame;
use log::{info, warn};
use pangolin_core::AccessLogEntry;
use pangolin_core::BotLogEntry;
use pangolin_core::RefererLogEntry;
use serde_json::json;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;

use crate::App;
use crate::serve::AdminBody;

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
/// hands the entry out by value. `T: Send` is one too: the stream is
/// held by the connection task, which the host runtime spawns.
///
/// The parameter list is long because the three streams pass their
/// own channel, snapshot and encoder; grouping them into a struct
/// would only move the same values one level down.
#[allow(clippy::too_many_arguments)]
async fn handle_stream<T, Sub, Rep, Enc>(
    app: Arc<App>,
    sessions: Arc<::admin::state::SessionStore>,
    cookie: Option<&str>,
    shutdown: CancellationToken,
    topic: &'static str,
    subscribe: Sub,
    replay: Rep,
    encode: Enc,
) -> hyper::Response<AdminBody>
where
    T: Clone + Send + 'static,
    Sub: FnOnce(&App) -> broadcast::Receiver<T> + Send,
    Rep: FnOnce(&App) -> Vec<T> + Send,
    Enc: Fn(&T) -> Bytes + Send + Sync + 'static,
{
    // 1) Auth — admin-only. Non-admin attempts get a normal 401
    //    JSON response, not an SSE frame.
    let token = cookie.and_then(::admin::state::parse_session_cookie);
    let authed = match token {
        Some(t) => sessions.validate(&t).await,
        None => false,
    };
    if !authed {
        return unauthorized(topic);
    }

    // 2) Subscribe FIRST, then snapshot. Subscribing first guarantees
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
    let rx = subscribe(&app);
    let snapshot = replay(&app);

    // 3) The frames that are already known: the replay and the
    //    comment that tells the operator how much of it there was.
    let mut pending: VecDeque<Bytes> = VecDeque::new();
    if snapshot.is_empty() {
        // Tell the client there is no replay. SSE comments are
        // ignored by `EventSource` but visible in devtools, which
        // is exactly the right surface for an operator debugging a
        // missing-events report.
        pending.push_back(frame_comment("replay: empty"));
    } else {
        // Capture the length BEFORE the `for` loop consumes the
        // Vec — moving `snapshot` into the iterator drops the
        // value before we can read `.len()` again.
        let snapshot_len = snapshot.len();
        for entry in snapshot {
            pending.push_back(encode(&entry));
        }
        pending.push_back(frame_comment(&format!("replay: done ({})", snapshot_len)));
    }

    let state = SseState {
        rx,
        pending,
        shutdown,
        topic,
        encode,
        done: false,
    };

    // The stream is pulled by hyper: every `poll_next` yields one
    // frame — a replayed entry, a live entry, or a comment.
    let frames = futures_util::stream::unfold(state, |mut state| async move {
        if let Some(frame) = state.pending.pop_front() {
            return Some((Ok(Frame::data(frame)), state));
        }
        if state.done {
            return None;
        }
        // Live broadcast loop. Race the broadcast receiver against the
        // shutdown token so a Ctrl-C during a long-lived SSE
        // connection drops the connection promptly instead of waiting
        // for the full drain window.
        let next = tokio::select! {
            // Bias toward shutdown so a Ctrl-C during a long
            // broadcast gap doesn't have to wait for the next entry
            // to arrive.
            biased;
            _ = state.shutdown.cancelled() => {
                info!("SSE: [{}] shutdown observed, ending live tail", state.topic);
                state.done = true;
                Some(frame_comment("shutdown"))
            }
            recv = state.rx.recv() => match recv {
                Ok(entry) => Some((state.encode)(&entry)),
                Err(RecvError::Lagged(n)) => {
                    // Subscriber fell behind. Surface as an SSE
                    // comment (browsers ignore these, devtools shows
                    // them). The next successful recv() will deliver
                    // the newest message the channel still has.
                    warn!("SSE: [{}] subscriber lagged by {n} events", state.topic);
                    Some(frame_comment(&format!("lagged {n} events")))
                }
                Err(RecvError::Closed) => {
                    // Broadcast channel closed (app shutdown). Send a
                    // terminal comment and end the stream cleanly.
                    state.done = true;
                    Some(frame_comment("closed"))
                }
            },
        };
        next.map(|frame| (Ok(Frame::data(frame)), state))
    });

    let body = http_body_util::StreamBody::new(frames).boxed_unsync();

    // The SSE prelude.
    //
    // We deliberately do NOT set `Transfer-Encoding: chunked` here:
    // hyper frames the body itself when the length is unknown, and
    // on HTTP/2 the header is forbidden entirely.
    let response = hyper::Response::builder()
        .status(http::StatusCode::OK)
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
        .header("Connection", "keep-alive")
        .body(body);
    match response {
        Ok(r) => r,
        Err(e) => {
            // The prelude is fixed and valid, so this cannot happen —
            // but a client must not be told "unauthenticated" because
            // the server had a bad day.
            warn!("SSE: [{topic}] failed to build response: {e}");
            crate::serve::respond(500, "text/plain", Bytes::new(), &[])
        }
    }
}

/// The state one SSE stream carries from frame to frame.
struct SseState<T, Enc> {
    /// The broadcast channel the live entries arrive on.
    rx: broadcast::Receiver<T>,
    /// The frames that are already known — the replay and its
    /// comment.
    pending: VecDeque<Bytes>,
    /// Cancelled when the gateway shuts down.
    shutdown: CancellationToken,
    /// The name of the stream, for the log lines.
    topic: &'static str,
    /// Turns one entry into a `data:` frame.
    encode: Enc,
    /// Whether the stream has said its last word.
    done: bool,
}

/// Handle the `/api/logs/stream` SSE request.
///
/// The stream itself is implemented by `handle_stream`; this
/// wrapper only names the access-log channel, snapshot and encoder.
///
/// `shutdown` is the gateway's shared cancellation token — the same
/// one SIGINT and SIGTERM cancel. We `select!` on it inside the live
/// broadcast loop so a Ctrl-C doesn't have to wait for the drain
/// window to expire before the SSE handler yields. Without this, an
/// idle SSE client (no new access-log entries) would hold the
/// runtime open for the whole drain — visible in the log as
/// `Waiting for service runtime pangolin-http to exit` — and the
/// operator-facing shutdown latency would be the drain budget
/// instead of the grace period alone.
pub async fn handle_access_log_stream(
    app: Arc<App>,
    sessions: Arc<::admin::state::SessionStore>,
    cookie: Option<&str>,
    shutdown: CancellationToken,
) -> hyper::Response<AdminBody> {
    handle_stream(
        app,
        sessions,
        cookie,
        shutdown,
        "access logs",
        |app| app.access_log_tx.subscribe(),
        |app| app.recent_access_log(),
        // The access-log entry travels as an `Arc` — one allocation,
        // shared by the ring buffer, the channel and every subscriber
        // (issue #224). The frame is built from the entry it points
        // at; the JSON is the same either way.
        |entry: &Arc<AccessLogEntry>| frame_data(entry),
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
    app: Arc<App>,
    sessions: Arc<::admin::state::SessionStore>,
    cookie: Option<&str>,
    shutdown: CancellationToken,
) -> hyper::Response<AdminBody> {
    handle_stream(
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
    app: Arc<App>,
    sessions: Arc<::admin::state::SessionStore>,
    cookie: Option<&str>,
    shutdown: CancellationToken,
) -> hyper::Response<AdminBody> {
    handle_stream(
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
/// straight to the response body without an extra copy.
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
fn unauthorized(topic: &str) -> hyper::Response<AdminBody> {
    let body = json!({
        "error": "unauthenticated",
        "message": format!("Admin login required to view {topic}."),
    })
    .to_string();
    crate::serve::respond(
        http::StatusCode::UNAUTHORIZED.as_u16(),
        "application/json",
        Bytes::from(body),
        &[("WWW-Authenticate", "Cookie")],
    )
}
