//! Yamux-over-fastwebsockets tunnel transport (issue #39).
//!
//! ## Layered architecture
//!
//! ```text
//!                 raw HTTP/1.1 bytes (or WS frame bytes)
//!                           │
//!                           ▼
//!                  tokio::io::copy_bidirectional      ← HTTP path
//!                  hand-written half-close pump       ← WS relay
//!                           │
//!                           ▼
//!                 yamux::Stream (one per request/conn)
//!                           │
//!                           ▼
//!                 yamux::Session
//!                           │
//!                           ▼
//!              tokio::io::DuplexStream   ← yamux's "transport"
//!                           │
//!                           ▼
//!              WsBridge task: pumps bytes between
//!              the DuplexStream and the fastwebsockets
//!              WebSocket, dealing with frame boundaries.
//!                           │
//!                           ▼
//!                       tokio TCP
//! ```
//!
//! ## Design choices
//!
//! * **yamux** is HashiCorp's compact 12-byte-header multiplexer
//!   (used by libp2p / Consul / Nomad). Its sliding-window flow
//!   control is what the old msgpack frame protocol lacked.
//! * **fastwebsockets** is a 2-4×-faster zero-copy WS parser.
//!   The `permessage-deflate` extension is built in and tuned
//!   for tun (sliding window).
//! * **Authorization: Bearer** is the auth channel. Validation
//!   runs in the WS server's upgrade callback. The callback is
//!   sync, so we use `tokio::task::block_in_place` to call the
//!   async DB lookup — only valid because the host runtime is
//!   `new_multi_thread`.
//! * **One stream per HTTP request / WS connection** — no
//!   shared-stream multiplexing, no rid correlation. Eliminates
//!   the msgpack double-parse (ngx struct → tun re-serialize).
//! * **Hop-by-op header stripping** is the ngx side's
//!   responsibility (RFC 7230 §6.1) when re-serializing the
//!   HTTP/1.1 byte stream to the backend.
//!
//! ## Failure modes
//!
//! * A torn yamux stream is signalled to the peer as RST, not
//!   FIN. The peer sees EOF on the read side and surfaces the
//!   5xx to the client.
//! * Auth rejection (401) returns a real HTTP 401 response and
//!   closes the TCP socket; it does NOT upgrade to WS.
//! * WS relay half-close: when one side sends EOF, the
//!   half-close pump shuts down that direction of the *other*
//!   leg and continues forwarding bytes for the still-open
//!   direction.

use std::io::{Error, ErrorKind, Result as IoResult};
use std::sync::Arc;

use fastwebsockets::{Frame, OpCode, Payload, WebSocket};
use log::debug;
use std::task::Context;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

pub use tokio_yamux as yamux;
use yamux::{Config as YamuxConfig, Session, StreamHandle};

// Re-exports for the manual WS handshake helpers below.
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use sha1::{Digest, Sha1};

/// Marker for which side of a tunnel we are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TunnelRole {
    /// ngx — accepts inbound WS, runs yamux server.
    Server,
    /// tun — dials outbound WS, runs yamux client.
    Client,
}

/// yamux session config tuned for the HTTP-reverse-proxy
/// traffic shape Pangolin sees. Defaults are mostly fine —
/// `max_stream_count` is set explicitly so the limit is part
/// of the code we own (not a yamux-default that could change
/// between versions).
fn yamux_config() -> YamuxConfig {
    YamuxConfig {
        max_stream_count: 4096,
        ..YamuxConfig::default()
    }
}

/// A live tunnel: yamux session over a fastwebsockets
/// WebSocket, glued by a `WsBridge` task.
///
/// The session is owned by a single task (spawned by
/// `tunnel_over_websocket`) and is driven from there.
/// `open_stream` and `accept_stream` send commands to the
/// driver task via mpsc channels; the driver task holds
/// the `&mut Session` it needs to make those calls and
/// pumps the session's `Stream` impl to dispatch incoming
/// stream-open events back to callers waiting on
/// `accept_stream`. This avoids the Mutex<Session>
/// approach (where a long-poll on `next()` would block any
/// other caller from making progress) and keeps the
/// session's invariants in one place.
pub struct YamuxTunnel {
    /// Channel for sending `open_stream` requests to the
    /// driver task. Each request is a oneshot that the
    /// driver replies to with the resulting
    /// `Result<StreamHandle, Error>`.
    open_tx: mpsc::UnboundedSender<OpenRequest>,
    /// Channel for sending `accept_stream` requests. The
    /// driver task maintains a queue of pending accepts
    /// and routes incoming stream-open events to the
    /// next waiting caller.
    accept_tx: mpsc::UnboundedSender<oneshot::Sender<IoResult<StreamHandle>>>,
    /// Which role this endpoint plays.
    pub role: TunnelRole,
    /// Abort handle for the bridge/driver task. Held for
    /// the lifetime of the tunnel; aborting it forces
    /// both down.
    pub bridge_abort: tokio::task::AbortHandle,
    /// Notification fired by the driver when the session
    /// ends (the WebSocket EOF'd, or the session saw a
    /// go-away). Callers that need to wait for the
    /// tunnel to close `.await` this notify.
    pub session_end: Arc<tokio::sync::Notify>,
}

struct OpenRequest {
    reply: oneshot::Sender<Result<StreamHandle, yamux::Error>>,
}

impl std::fmt::Debug for YamuxTunnel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("YamuxTunnel")
            .field("role", &self.role)
            .finish()
    }
}

impl Clone for YamuxTunnel {
    fn clone(&self) -> Self {
        Self {
            open_tx: self.open_tx.clone(),
            accept_tx: self.accept_tx.clone(),
            role: self.role,
            bridge_abort: self.bridge_abort.clone(),
            session_end: self.session_end.clone(),
        }
    }
}

impl YamuxTunnel {
    /// Open a new outbound yamux stream. Each HTTP request or
    /// WS connection gets its own stream.
    pub async fn open_stream(&self) -> IoResult<StreamHandle> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.open_tx
            .send(OpenRequest { reply: reply_tx })
            .map_err(|_| Error::new(ErrorKind::BrokenPipe, "tunnel closed"))?;
        match reply_rx.await {
            Ok(Ok(s)) => Ok(s),
            Ok(Err(e)) => Err(Error::other(format!("open_stream: {e}"))),
            Err(_) => Err(Error::new(ErrorKind::BrokenPipe, "open_stream: no reply")),
        }
    }

    /// Accept the next inbound yamux stream. Returns
    /// `None` when the session has ended.
    pub async fn accept_stream(&self) -> Option<IoResult<StreamHandle>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self.accept_tx.send(reply_tx).is_err() {
            return None;
        }
        match reply_rx.await {
            Ok(Ok(s)) => Some(Ok(s)),
            Ok(Err(e)) => Some(Err(Error::other(format!("accept_stream: {e}")))),
            Err(_) => None,
        }
    }
}

/// Build a `YamuxTunnel` over an already-handshaken
/// `fastwebsockets::WebSocket<S>`.
///
/// The `WebSocket` is moved into a `WsBridge` task that
/// ferries bytes between the WS frame protocol and a
/// `tokio::io::DuplexStream`. Yamux operates on the duplex
/// stream, which is an in-memory byte pipe that quacks like
/// `AsyncRead + AsyncWrite + Unpin`.
pub fn tunnel_over_websocket<S>(ws: WebSocket<S>, role: TunnelRole) -> YamuxTunnel
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (yamux_side, ws_side) = tokio::io::duplex(64 * 1024);
    let session = match role {
        TunnelRole::Server => Session::new_server(yamux_side, yamux_config()),
        TunnelRole::Client => Session::new_client(yamux_side, yamux_config()),
    };
    let (open_tx, open_rx) = mpsc::unbounded_channel::<OpenRequest>();
    let (accept_tx, accept_rx) =
        mpsc::unbounded_channel::<oneshot::Sender<IoResult<StreamHandle>>>();
    let session_end = Arc::new(tokio::sync::Notify::new());
    let session_end_for_task = session_end.clone();

    // Bridge + driver in one task: the bridge ferries bytes
    // between the WebSocket and the duplex, while the
    // driver owns the `Session` exclusively and pumps its
    // `Stream` impl to dispatch accept events to callers
    // waiting on `accept_stream`. Because the session is
    // owned by a single task, there is no Mutex<Session>
    // — and no risk of a long-poll on `next()` blocking
    // other callers.
    let bridge = tokio::spawn(async move {
        // Bridge the WS and the duplex.
        let bridge_fut = ws_bridge(ws, ws_side);
        // Drive the session.
        let driver_fut = session_driver(session, open_rx, accept_rx);
        // Whichever finishes first cancels the other.
        tokio::select! {
            _ = bridge_fut => {}
            _ = driver_fut => {}
        }
        session_end_for_task.notify_waiters();
    });
    let bridge_abort = bridge.abort_handle();
    YamuxTunnel {
        open_tx,
        accept_tx,
        role,
        bridge_abort,
        session_end,
    }
}

/// Single-task driver for a `yamux::Session` over a
/// `tokio::io::DuplexStream`. Owns the session; pumps its
/// `Stream` impl to route incoming stream-open events to
/// callers waiting on `accept_stream`; processes
/// `open_stream` requests received on `open_rx`.
async fn session_driver(
    mut session: Session<tokio::io::DuplexStream>,
    mut open_rx: mpsc::UnboundedReceiver<OpenRequest>,
    mut accept_rx: mpsc::UnboundedReceiver<oneshot::Sender<IoResult<StreamHandle>>>,
) {
    use futures_util::StreamExt;
    let mut pending_accepts: std::collections::VecDeque<oneshot::Sender<IoResult<StreamHandle>>> =
        std::collections::VecDeque::new();
    loop {
        tokio::select! {
            biased;
            // Open-stream requests from callers.
            req = open_rx.recv() => {
                let Some(req) = req else { break; };
                let result = session.open_stream();
                let _ = req.reply.send(result);
            }
            // Accept-stream requests from callers.
            req = accept_rx.recv() => {
                let Some(req) = req else { break; };
                pending_accepts.push_back(req);
            }
            // Incoming yamux events: an opened stream
            // (server side) is dispatched to the next
            // pending accept.
            ev = session.next() => {
                match ev {
                    Some(Ok(stream)) => {
                        if let Some(reply) = pending_accepts.pop_front() {
                            let _ = reply.send(Ok(stream));
                        }
                        // If nobody is waiting, drop the
                        // stream — the remote opener will
                        // see a RST.
                    }
                    Some(Err(_e)) => {
                        // Surface the error to the next
                        // pending acceptor and continue.
                        if let Some(reply) = pending_accepts.pop_front() {
                            let _ = reply.send(Err(Error::other(format!("accept error: {_e}"))));
                        }
                    }
                    None => break,
                }
            }
        }
    }
    // Session ended: drain pending acceptors with an EOF
    // error so they don't hang.
    while let Some(reply) = pending_accepts.pop_front() {
        let _ = reply.send(Err(Error::new(
            ErrorKind::UnexpectedEof,
            "yamux session ended",
        )));
    }
}

// ---------------------------------------------------------------------------
// WsBridge: ferries bytes between a WebSocket and a DuplexStream.
// ---------------------------------------------------------------------------

/// Bridge task: pump bytes from a `WebSocket` to a
/// `DuplexStream` (in both directions).
///
/// Each WebSocket Binary frame is a chunk of bytes on the
/// duplex stream; the receiving side reads them
/// transparently. The bridge is symmetric: the *y* end of
/// the duplex is owned by yamux; the *s* end is owned by
/// this task. On EOF from either side, the task ends.
async fn ws_bridge<S>(mut ws: WebSocket<S>, mut s: tokio::io::DuplexStream)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut buffer = vec![0u8; 16 * 1024];
    // We need two pump directions in one task. We use
    // tokio::select! on (read WS frame, read from duplex).
    // When the duplex's reader returns EOF, the bridge
    // sends a Close frame on the WS and ends. When the
    // WS read returns None (Close), the bridge shuts
    // down the duplex writer and ends.
    let mut ws_closed = false;
    let mut duplex_closed = false;
    loop {
        if ws_closed && duplex_closed {
            return;
        }
        tokio::select! {
            // duplex → WS
            res = s.read(&mut buffer), if !duplex_closed => {
                match res {
                    Ok(0) => {
                        // yamux closed its end → send Close frame
                        let close_frame = Frame::new(true, OpCode::Close, None, Payload::Owned(Vec::new()));
                        if ws.write_frame(close_frame).await.is_err() {
                            return;
                        }
                        duplex_closed = true;
                    }
                    Ok(n) => {
                        let frame = Frame::new(true, OpCode::Binary, None, Payload::Borrowed(&buffer[..n]));
                        if ws.write_frame(frame).await.is_err() {
                            return;
                        }
                    }
                    Err(_) => {
                        let _ = ws.write_frame(Frame::new(true, OpCode::Close, None, Payload::Owned(Vec::new()))).await;
                        return;
                    }
                }
            }
            // WS → duplex
            res = ws.read_frame(), if !ws_closed => {
                match res {
                    Ok(frame) => match frame.opcode {
                        OpCode::Binary => {
                            let payload = frame.payload.to_vec();
                            if s.write_all(&payload).await.is_err() {
                                return;
                            }
                        }
                        OpCode::Close => {
                            let _ = s.shutdown().await;
                            ws_closed = true;
                        }
                        OpCode::Ping | OpCode::Pong => {
                            // auto-pong handled by fastwebsockets
                        }
                        other => {
                            debug!("ws bridge: ignoring opcode {:?}", other);
                        }
                    },
                    Err(_) => {
                        let _ = s.shutdown().await;
                        return;
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP / WS byte-pipe helpers
// ---------------------------------------------------------------------------

/// Read a complete HTTP/1.1 request (headers + body) from a
/// yamux stream and return the parsed head + body bytes.
pub async fn read_http_request<R>(r: &mut R) -> IoResult<HttpRequest>
where
    R: AsyncRead + Unpin,
{
    let mut header_buf = Vec::with_capacity(2048);
    let mut tmp = [0u8; 1024];
    loop {
        let n = r.read(&mut tmp).await?;
        if n == 0 {
            return Err(Error::new(ErrorKind::UnexpectedEof, "eof in headers"));
        }
        header_buf.extend_from_slice(&tmp[..n]);
        if let Some(idx) = find_header_end(&header_buf) {
            let head_end = idx + 4;
            let body_prefix = header_buf[head_end..].to_vec();
            let head_bytes = &header_buf[..head_end];
            let head = std::str::from_utf8(head_bytes)
                .map_err(|e| Error::new(ErrorKind::InvalidData, format!("utf-8: {e}")))?;
            let req = parse_request_head(head)?;
            let body = read_http_body_kind(r, &req.body_kind, body_prefix).await?;
            return Ok(HttpRequest {
                method: req.method,
                target: req.target,
                version: req.version,
                headers: req.headers,
                body,
            });
        }
        if header_buf.len() > 64 * 1024 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "header block >64 KiB without CRLFCRLF",
            ));
        }
    }
}

/// Parse a complete HTTP/1.1 request from a byte slice (synchronous
/// variant of [`read_http_request`]). Used by the tunnel frame
/// decoder, which already has the entire request as a single
/// `&[u8]` (no streaming reader).
pub fn parse_http_request_bytes(bytes: &[u8]) -> IoResult<HttpRequest> {
    let idx = find_header_end(bytes)
        .ok_or_else(|| Error::new(ErrorKind::InvalidData, "no CRLFCRLF in request bytes"))?;
    let head_end = idx + 4;
    let head_bytes = &bytes[..head_end];
    let body_prefix = &bytes[head_end..];
    let head = std::str::from_utf8(head_bytes)
        .map_err(|e| Error::new(ErrorKind::InvalidData, format!("utf-8: {e}")))?;
    let parsed = parse_request_head(head)?;
    // Body: read from body_prefix (in-memory) instead of an
    // AsyncRead. For Length(n) we copy n bytes; for Chunked we
    // decode; for UntilEof we copy everything after the head.
    let body = match parsed.body_kind {
        BodyKind::Length(n) => {
            if body_prefix.len() < n {
                return Err(Error::new(
                    ErrorKind::UnexpectedEof,
                    format!("short body: expected {} got {}", n, body_prefix.len()),
                ));
            }
            body_prefix[..n].to_vec()
        }
        BodyKind::Chunked => {
            let mut out = Vec::new();
            let mut cursor = 0usize;
            loop {
                let crlf = find_crlf(&body_prefix[cursor..])
                    .ok_or_else(|| Error::new(ErrorKind::InvalidData, "chunked: no size CRLF"))?;
                let size_line = std::str::from_utf8(&body_prefix[cursor..cursor + crlf])
                    .map_err(|e| Error::new(ErrorKind::InvalidData, format!("utf-8: {e}")))?;
                let size_str = size_line.split(';').next().unwrap_or("").trim();
                let size = usize::from_str_radix(size_str, 16)
                    .map_err(|e| Error::new(ErrorKind::InvalidData, format!("chunk size: {e}")))?;
                cursor += crlf + 2;
                if size == 0 {
                    // skip trailers + final CRLF
                    let _ = find_crlf(&body_prefix[cursor..]);
                    break;
                }
                if body_prefix.len() < cursor + size + 2 {
                    return Err(Error::new(ErrorKind::UnexpectedEof, "chunked: short body"));
                }
                out.extend_from_slice(&body_prefix[cursor..cursor + size]);
                cursor += size + 2;
            }
            out
        }
        BodyKind::UntilEof => body_prefix.to_vec(),
    };
    Ok(HttpRequest {
        method: parsed.method,
        target: parsed.target,
        version: parsed.version,
        headers: parsed.headers,
        body,
    })
}

fn find_crlf(bytes: &[u8]) -> Option<usize> {
    bytes.windows(2).position(|w| w == b"\r\n")
}

/// Serialise an `HttpRequest` back into a wire-format HTTP/1.1
/// request byte buffer.
pub fn encode_http_request(req: &HttpRequest) -> Vec<u8> {
    let mut out = Vec::with_capacity(256 + req.body.len());
    out.extend_from_slice(req.method.as_bytes());
    out.extend_from_slice(b" ");
    out.extend_from_slice(req.target.as_bytes());
    out.extend_from_slice(b" ");
    out.extend_from_slice(req.version.as_bytes());
    out.extend_from_slice(b"\r\n");
    for (k, v) in &req.headers {
        out.extend_from_slice(k.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(v.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    if !has_header(&req.headers, "content-length") && !has_header(&req.headers, "transfer-encoding")
    {
        out.extend_from_slice(format!("Content-Length: {}\r\n", req.body.len()).as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(&req.body);
    out
}

/// Serialise an `HttpResponse` back into a wire-format HTTP/1.1
/// response byte buffer.
/// Decode an `HttpResponse` from a byte slice produced by `encode_http_response`.
/// Returns `None` if the bytes are empty.
///
/// **Why we don't `from_utf8` the whole buffer**: the response body is
/// arbitrary bytes (e.g. `Content-Encoding: deflate` returns a 5-byte
/// deflate stream like `01 00 00 ff ff`, or any binary download).
/// Validating the whole buffer as UTF-8 was a real bug — see the
/// `decode_http_response_does_not_validate_body_as_utf8` test.
///
/// The **header** section is required to be ASCII (RFC 7230 §3.2.4:
/// "field-value    = *( field-content / obs-fold )" where
/// `field-content  = field-vchar [ 1*( SP / HTAB ) field-vchar ]`
/// and `field-vchar  = VCHAR / obs-text`, with VCHAR = `%x21-7E`).
/// We still validate the header section as UTF-8 (a strict subset of
/// ASCII in well-formed responses); the body is kept as `Vec<u8>`.
pub fn decode_http_response(bytes: &[u8]) -> IoResult<Option<HttpResponse>> {
    if bytes.is_empty() {
        return Ok(None);
    }

    // Format: "HTTP/1.1 {status_line}\r\n{headers}\r\n\r\n{body}"
    let split = bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "no header/body split")
        })?;

    // Validate **only the header section** as UTF-8. Body stays as
    // `Vec<u8>` so binary content (deflate, gzip, images, downloads)
    // passes through untouched.
    let header_str = std::str::from_utf8(&bytes[..split]).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("response header is not valid UTF-8: {e}"),
        )
    })?;
    let body_bytes = bytes[split + 4..].to_vec();

    let mut lines = header_str.splitn(2, "\r\n");
    let first_line = lines.next().unwrap_or("");
    // first_line: "HTTP/1.1 200 OK"
    let mut parts = first_line.splitn(2, ' ');
    let version = parts.next().unwrap_or("HTTP/1.1").to_string();
    let status_line = parts.next().unwrap_or("502 Bad Gateway").to_string();

    let headers_part = lines.next().unwrap_or("");
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in headers_part.split("\r\n") {
        if let Some(pos) = line.find(": ") {
            headers.push((line[..pos].to_string(), line[pos + 2..].to_string()));
        }
    }

    Ok(Some(HttpResponse {
        version,
        status_line,
        headers,
        body: body_bytes,
    }))
}

pub fn encode_http_response(resp: &HttpResponse) -> Vec<u8> {
    let mut out = Vec::with_capacity(128 + resp.body.len());
    out.extend_from_slice(resp.version.as_bytes());
    out.extend_from_slice(b" ");
    out.extend_from_slice(resp.status_line.as_bytes());
    out.extend_from_slice(b"\r\n");
    for (k, v) in &resp.headers {
        out.extend_from_slice(k.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(v.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    if !has_header(&resp.headers, "content-length")
        && !has_header(&resp.headers, "transfer-encoding")
    {
        out.extend_from_slice(format!("Content-Length: {}\r\n", resp.body.len()).as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(&resp.body);
    out
}

/// Read a complete HTTP/1.1 response off a yamux stream.
pub async fn read_http_response<R>(r: &mut R) -> IoResult<HttpResponse>
where
    R: AsyncRead + Unpin,
{
    let mut header_buf = Vec::with_capacity(2048);
    let mut tmp = [0u8; 1024];
    loop {
        let n = r.read(&mut tmp).await?;
        if n == 0 {
            return Err(Error::new(ErrorKind::UnexpectedEof, "eof in headers"));
        }
        header_buf.extend_from_slice(&tmp[..n]);
        if let Some(idx) = find_header_end(&header_buf) {
            let head_end = idx + 4;
            let body_prefix = header_buf[head_end..].to_vec();
            let head_bytes = &header_buf[..head_end];
            let head = std::str::from_utf8(head_bytes)
                .map_err(|e| Error::new(ErrorKind::InvalidData, format!("utf-8: {e}")))?;
            let resp = parse_response_head(head)?;
            let body = read_http_body_kind(r, &resp.body_kind, body_prefix).await?;
            return Ok(HttpResponse {
                version: resp.version,
                status_line: resp.status_line,
                headers: resp.headers,
                body,
            });
        }
        if header_buf.len() > 64 * 1024 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "header block >64 KiB without CRLFCRLF",
            ));
        }
    }
}

/// Strip RFC 7230 §6.1 hop-by-hop headers. Called by the ngx
/// side before re-serialising the HTTP request bytes to the
/// backend.
pub fn strip_hop_by_hop_headers(headers: &mut Vec<(String, String)>) {
    fn is_hop_by_hop(name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        matches!(
            lower.as_str(),
            "connection"
                | "keep-alive"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "trailer"
                | "transfer-encoding"
                | "upgrade"
        ) || lower.starts_with("proxy-")
    }
    headers.retain(|(k, _)| !is_hop_by_hop(k));
}

// ---- internal HTTP head/body types ----

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    pub target: String,
    pub version: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub version: String,
    pub status_line: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Debug)]
struct RequestHead {
    method: String,
    target: String,
    version: String,
    headers: Vec<(String, String)>,
    body_kind: BodyKind,
}

#[derive(Debug)]
struct ResponseHead {
    version: String,
    status_line: String,
    headers: Vec<(String, String)>,
    body_kind: BodyKind,
}

#[derive(Debug, Clone)]
enum BodyKind {
    Length(usize),
    Chunked,
    UntilEof,
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_request_head(head: &str) -> IoResult<RequestHead> {
    let mut lines = head.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| Error::new(ErrorKind::InvalidData, "missing request line"))?;
    let mut parts = request_line.splitn(3, ' ');
    let method = parts
        .next()
        .ok_or_else(|| Error::new(ErrorKind::InvalidData, "missing method"))?
        .to_string();
    let target = parts
        .next()
        .ok_or_else(|| Error::new(ErrorKind::InvalidData, "missing target"))?
        .to_string();
    let version = parts
        .next()
        .ok_or_else(|| Error::new(ErrorKind::InvalidData, "missing version"))?
        .to_string();
    let mut headers = Vec::new();
    let mut body_kind = BodyKind::UntilEof;
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_string();
            let v = v.trim().to_string();
            let lower = k.to_ascii_lowercase();
            if lower == "content-length" {
                if let Ok(n) = v.parse::<usize>() {
                    body_kind = BodyKind::Length(n);
                }
            } else if lower == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") {
                body_kind = BodyKind::Chunked;
            }
            headers.push((k, v));
        }
    }
    Ok(RequestHead {
        method,
        target,
        version,
        headers,
        body_kind,
    })
}

fn parse_response_head(head: &str) -> IoResult<ResponseHead> {
    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| Error::new(ErrorKind::InvalidData, "missing status line"))?
        .to_string();
    let mut parts = status_line.splitn(3, ' ');
    let version = parts
        .next()
        .ok_or_else(|| Error::new(ErrorKind::InvalidData, "missing version"))?
        .to_string();
    let _ = parts.next();
    let mut headers = Vec::new();
    let mut body_kind = BodyKind::UntilEof;
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_string();
            let v = v.trim().to_string();
            let lower = k.to_ascii_lowercase();
            if lower == "content-length" {
                if let Ok(n) = v.parse::<usize>() {
                    body_kind = BodyKind::Length(n);
                }
            } else if lower == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") {
                body_kind = BodyKind::Chunked;
            }
            headers.push((k, v));
        }
    }
    Ok(ResponseHead {
        version,
        status_line,
        headers,
        body_kind,
    })
}

async fn read_http_body_kind<R>(
    r: &mut R,
    body_kind: &BodyKind,
    already: Vec<u8>,
) -> IoResult<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    match body_kind {
        BodyKind::Length(n) => {
            let mut out = already;
            out.reserve(*n);
            while out.len() < *n {
                let need = *n - out.len();
                let mut tmp = vec![0u8; need.min(8192)];
                let read = r.read(&mut tmp).await?;
                if read == 0 {
                    return Err(Error::new(
                        ErrorKind::UnexpectedEof,
                        format!("short body: expected {} got {}", n, out.len()),
                    ));
                }
                out.extend_from_slice(&tmp[..read]);
            }
            Ok(out)
        }
        BodyKind::Chunked => {
            let mut out = already;
            loop {
                let size_line = read_line(r).await?;
                let size_str = size_line.split(';').next().unwrap_or("").trim();
                let size = usize::from_str_radix(size_str, 16)
                    .map_err(|e| Error::new(ErrorKind::InvalidData, format!("chunk size: {e}")))?;
                if size == 0 {
                    loop {
                        let trailer = read_line(r).await?;
                        if trailer.is_empty() {
                            break;
                        }
                    }
                    return Ok(out);
                }
                let mut chunk = vec![0u8; size];
                r.read_exact(&mut chunk).await?;
                out.extend_from_slice(&chunk);
                let mut crlf = [0u8; 2];
                r.read_exact(&mut crlf).await?;
                if &crlf != b"\r\n" {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        "missing CRLF after chunk",
                    ));
                }
            }
        }
        BodyKind::UntilEof => {
            let mut out = already;
            let mut tmp = [0u8; 8192];
            loop {
                let n = r.read(&mut tmp).await?;
                if n == 0 {
                    return Ok(out);
                }
                out.extend_from_slice(&tmp[..n]);
            }
        }
    }
}

async fn read_line<R: AsyncRead + Unpin>(r: &mut R) -> IoResult<String> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1];
    loop {
        let n = r.read(&mut tmp).await?;
        if n == 0 {
            if buf.is_empty() {
                return Err(Error::new(ErrorKind::UnexpectedEof, "eof in chunked line"));
            }
            return Ok(String::from_utf8_lossy(&buf).into_owned());
        }
        if tmp[0] == b'\n' {
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            return Ok(String::from_utf8_lossy(&buf).into_owned());
        }
        buf.push(tmp[0]);
    }
}

fn has_header(headers: &[(String, String)], name: &str) -> bool {
    let target = name.to_ascii_lowercase();
    headers
        .iter()
        .any(|(k, _)| k.to_ascii_lowercase() == target)
}

// ---------------------------------------------------------------------------
// WS relay: hand-written half-close byte pump
// ---------------------------------------------------------------------------

/// Drive a WebSocket half-close byte pump over a yamux stream.
pub async fn pump_ws_relay<L, R>(
    mut local: L,
    mut remote: R,
    local_name: &'static str,
    peer_name: &'static str,
) -> IoResult<()>
where
    L: AsyncRead + AsyncWrite + Unpin,
    R: AsyncRead + AsyncWrite + Unpin,
{
    let mut buf_a = [0u8; 16 * 1024];
    let mut buf_b = [0u8; 16 * 1024];
    let mut local_closed = false;
    let mut remote_closed = false;

    loop {
        if local_closed && remote_closed {
            return Ok(());
        }
        tokio::select! {
            r = local.read(&mut buf_a), if !local_closed => {
                match r {
                    Ok(0) => {
                        let _ = remote.shutdown().await;
                        local_closed = true;
                        debug!("ws relay ({}/{}): local half closed", local_name, peer_name);
                    }
                    Ok(n) => {
                        if remote.write_all(&buf_a[..n]).await.is_err() {
                            let _ = local.shutdown().await;
                            return Ok(());
                        }
                    }
                    Err(_) => {
                        let _ = remote.shutdown().await;
                        local_closed = true;
                    }
                }
            }
            r = remote.read(&mut buf_b), if !remote_closed => {
                match r {
                    Ok(0) => {
                        let _ = local.shutdown().await;
                        remote_closed = true;
                        debug!("ws relay ({}/{}): remote half closed", local_name, peer_name);
                    }
                    Ok(n) => {
                        if local.write_all(&buf_b[..n]).await.is_err() {
                            let _ = remote.shutdown().await;
                            return Ok(());
                        }
                    }
                    Err(_) => {
                        let _ = local.shutdown().await;
                        remote_closed = true;
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// InsecureServerCertVerifier for `tls.verify = false`
// ---------------------------------------------------------------------------

/// rustls `ServerCertVerifier` that accepts any certificate.
pub mod insecure_verifier {
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, Error, SignatureScheme};

    #[derive(Debug, Default)]
    pub struct InsecureServerCertVerifier;

    impl ServerCertVerifier for InsecureServerCertVerifier {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }
}

// ---------------------------------------------------------------------------
// Manual WebSocket handshake helpers (server + client)
// ---------------------------------------------------------------------------

const WS_MAGIC: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Generate a random 16-byte base64-encoded
/// `Sec-WebSocket-Key` value (RFC 6455 §1.3).
pub fn generate_ws_key() -> String {
    use rand::Rng;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill(&mut bytes);
    BASE64.encode(bytes)
}

/// Compute the `Sec-WebSocket-Accept` value for the given
/// `Sec-WebSocket-Key` (RFC 6455 §1.3).
pub fn compute_ws_accept(key: &str) -> String {
    let mut sha = Sha1::new();
    sha.update(key.trim().as_bytes());
    sha.update(WS_MAGIC.as_bytes());
    BASE64.encode(sha.finalize())
}

/// A parsed minimal view of a WebSocket upgrade request.
#[derive(Debug, Clone)]
pub struct WsUpgradeRequest {
    pub method: String,
    pub target: String,
    pub version: String,
    pub authorization: Option<String>,
    pub sec_websocket_key: Option<String>,
}

/// Read a single WebSocket upgrade request off an arbitrary
/// `AsyncRead` stream.
pub async fn read_ws_upgrade_request<R>(r: &mut R) -> IoResult<WsUpgradeRequest>
where
    R: AsyncRead + Unpin,
{
    let mut buf = Vec::with_capacity(2048);
    let mut tmp = [0u8; 1024];
    loop {
        let n = r.read(&mut tmp).await?;
        if n == 0 {
            return Err(Error::new(
                ErrorKind::UnexpectedEof,
                "eof in upgrade headers",
            ));
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(idx) = find_header_end(&buf) {
            let head = std::str::from_utf8(&buf[..idx])
                .map_err(|e| Error::new(ErrorKind::InvalidData, format!("utf-8: {e}")))?;
            return Ok(parse_ws_upgrade_request(head));
        }
        if buf.len() > 16 * 1024 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "upgrade headers >16 KiB without CRLFCRLF",
            ));
        }
    }
}

fn parse_ws_upgrade_request(head: &str) -> WsUpgradeRequest {
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("").to_string();
    let mut parts = request_line.splitn(3, ' ');
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let version = parts.next().unwrap_or("HTTP/1.1").to_string();
    let mut authorization = None;
    let mut sec_websocket_key = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let k_lc = k.trim().to_ascii_lowercase();
            let v = v.trim().to_string();
            if k_lc == "authorization" {
                authorization = Some(v);
            } else if k_lc == "sec-websocket-key" {
                sec_websocket_key = Some(v);
            }
        }
    }
    WsUpgradeRequest {
        method,
        target,
        version,
        authorization,
        sec_websocket_key,
    }
}

/// Extract the `Bearer <token>` from an `Authorization: …`
/// header value.
pub fn bearer_token(authorization: Option<&str>) -> Option<String> {
    let v = authorization?;
    let mut parts = v.splitn(2, ' ');
    let scheme = parts.next()?;
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return None;
    }
    let token = parts.next()?.trim();
    if token.is_empty() {
        return None;
    }
    Some(token.to_string())
}

/// Write a `101 Switching Protocols` response to the stream.
pub async fn write_ws_accept_response<W>(w: &mut W, key: &str) -> IoResult<()>
where
    W: AsyncWrite + Unpin,
{
    let accept = compute_ws_accept(key);
    let resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n\
         \r\n",
        accept
    );
    w.write_all(resp.as_bytes()).await?;
    w.flush().await
}

/// Write an HTTP/1.1 error response and shut the stream down.
pub async fn write_http_error<W>(w: &mut W, status: u16, reason: &str) -> IoResult<()>
where
    W: AsyncWrite + Unpin,
{
    let body = format!("{status} {reason}\n");
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: text/plain\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        body.len(),
        body
    );
    w.write_all(resp.as_bytes()).await?;
    w.flush().await
}

/// Build a WebSocket upgrade request bytes for the client
/// side. Returns the full HTTP/1.1 request text. The caller
/// writes it to the stream and then awaits the 101 response.
pub fn build_ws_upgrade_request(path: &str, host: &str, token: &str) -> (Vec<u8>, String) {
    let key = generate_ws_key();
    let req = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         Authorization: Bearer {token}\r\n\
         \r\n"
    );
    (req.into_bytes(), key)
}

/// Read the 101 response off the stream and validate the
/// `Sec-WebSocket-Accept` value matches what we sent.
pub async fn read_ws_accept_response<R>(r: &mut R, expected_key: &str) -> IoResult<()>
where
    R: AsyncRead + Unpin,
{
    let mut buf = Vec::with_capacity(512);
    let mut tmp = [0u8; 512];
    loop {
        let n = r.read(&mut tmp).await?;
        if n == 0 {
            return Err(Error::new(ErrorKind::UnexpectedEof, "eof in 101 response"));
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(idx) = find_header_end(&buf) {
            let head = std::str::from_utf8(&buf[..idx])
                .map_err(|e| Error::new(ErrorKind::InvalidData, format!("utf-8: {e}")))?;
            let status_line = head.lines().next().unwrap_or("");
            if !status_line.contains(" 101 ") {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    format!("expected 101 Switching Protocols, got: {status_line}"),
                ));
            }
            let expected = compute_ws_accept(expected_key);
            if !head.lines().any(|l| {
                l.split_once(':')
                    .map(|(k, v)| {
                        k.trim().eq_ignore_ascii_case("sec-websocket-accept")
                            && v.trim() == expected
                    })
                    .unwrap_or(false)
            }) {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "Sec-WebSocket-Accept mismatch",
                ));
            }
            return Ok(());
        }
        if buf.len() > 16 * 1024 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "101 response headers >16 KiB",
            ));
        }
    }
}

pub use yamux::StreamHandle as YamuxStream;

// Re-export Role for callers that want to construct WebSocket
// objects manually.
pub use fastwebsockets::Role as WsRole;

// ---------------------------------------------------------------------------
// Convenience: build a WebSocket<tokio::net::TcpStream> after a successful
// manual client-side handshake.
// ---------------------------------------------------------------------------

/// Wrap an `AsyncRead+AsyncWrite` stream as a
/// `fastwebsockets::WebSocket<S>` after the WS upgrade has
/// already been performed manually. Used by both the tun
/// client and the ngx server after a successful handshake.
pub fn wrap_websocket<S>(stream: S, role: WsRole) -> WebSocket<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    WebSocket::after_handshake(stream, role)
}

// silence unused import warning when only some of the helpers
// are pulled in
#[allow(unused)]
fn _silence_unused() {
    let _ = std::marker::PhantomData::<Context<'_>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Bug-fix regression (issue: tunnel 502 on deflate body).**
    ///
    /// The pre-fix `decode_http_response` validated the *whole* response
    /// buffer (header + body) as UTF-8. That was wrong: a 302 with
    /// `Content-Encoding: deflate` has a 5-byte deflate body
    /// (`01 00 00 ff ff`), which is not valid UTF-8. The
    /// `from_utf8` check failed at `index 126` (first body byte) and
    /// ngx returned 502 to the browser.
    ///
    /// The fix splits the header/body at `\r\n\r\n` first and only
    /// validates the **header** section as UTF-8. Body bytes are
    /// preserved verbatim — exactly what an HTTP gateway must do for
    /// compressed/binary responses.
    #[test]
    fn decode_http_response_does_not_validate_body_as_utf8() {
        // Real bytes captured from the production 8888 backend
        // (`curl -H 'Accept-Encoding: deflate'`), 5 bytes of raw
        // deflate stream.
        let body: Vec<u8> = vec![0x01, 0x00, 0x00, 0xff, 0xff];
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"HTTP/1.1 302 Found\r\n");
        bytes.extend_from_slice(b"Content-Encoding: deflate\r\n");
        bytes.extend_from_slice(b"Location: /login\r\n");
        bytes.extend_from_slice(b"Date: Mon, 15 Jun 2026 09:06:25 GMT\r\n");
        bytes.extend_from_slice(b"Content-Length: 5\r\n");
        bytes.extend_from_slice(b"\r\n");
        bytes.extend_from_slice(&body);

        let resp = decode_http_response(&bytes)
            .expect("decode must succeed for binary body")
            .expect("non-empty response");

        assert_eq!(resp.status_line, "302 Found");
        assert_eq!(resp.body, body, "body bytes must be preserved verbatim");
        let loc = resp
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("location"))
            .expect("Location header present");
        assert_eq!(loc.1, "/login");
        let ce = resp
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-encoding"))
            .expect("Content-Encoding header present");
        assert_eq!(ce.1, "deflate");
    }

    /// Sanity: a well-formed 200 with a JSON body still decodes.
    #[test]
    fn decode_http_response_text_body_unchanged() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"HTTP/1.1 200 OK\r\n");
        bytes.extend_from_slice(b"Content-Type: application/json\r\n");
        bytes.extend_from_slice(b"Content-Length: 2\r\n");
        bytes.extend_from_slice(b"\r\n");
        bytes.extend_from_slice(b"{}");
        let resp = decode_http_response(&bytes).unwrap().unwrap();
        assert_eq!(resp.status_line, "200 OK");
        assert_eq!(resp.body, b"{}");
    }

    /// A response with **no body** (the common 302 + Location case)
    /// must still decode cleanly.
    #[test]
    fn decode_http_response_no_body() {
        let bytes = b"HTTP/1.1 302 Found\r\nLocation: /login\r\nContent-Length: 0\r\n\r\n";
        let resp = decode_http_response(bytes).unwrap().unwrap();
        assert_eq!(resp.status_line, "302 Found");
        assert!(resp.body.is_empty());
    }

    /// Garbage bytes in the **header** section must still fail —
    /// we only relaxed the body check, not the header check.
    #[test]
    fn decode_http_response_rejects_non_utf8_header() {
        // Inject a non-UTF-8 byte (0x80) into a header value.
        let bytes = b"HTTP/1.1 200 OK\r\nX-Bad: \x80\x81\r\n\r\n";
        let err = decode_http_response(bytes).expect_err("non-utf8 header must fail");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    /// Empty buffer → None (preserves existing behaviour).
    #[test]
    fn decode_http_response_empty() {
        assert!(decode_http_response(b"").unwrap().is_none());
    }
}
