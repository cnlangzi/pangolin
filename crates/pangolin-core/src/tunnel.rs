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

// ---------------------------------------------------------------------------
// Transport seam (issue #153)
// ---------------------------------------------------------------------------

/// Wire-format version of the tunnel protocol.
///
/// Issue #153 replaced the request/response frame protocol with a raw
/// byte splice: a stream carries a single `CONNECT` instruction and
/// then nothing but bytes. A tun and a gateway that disagree on this
/// number speak different protocols, so both sides refuse the
/// connection with a message naming **both** versions instead of
/// failing later inside the protocol with an opaque error.
///
/// The version travels in its own handshake on each transport: as an
/// extra header of the WebSocket upgrade request (TCP fallback) and in
/// the `AUTH` control line (QUIC).
pub const TUNNEL_PROTOCOL_VERSION: u32 = 2;

/// The QUIC transport (issue #153) — the tunnel's primary transport.
pub mod quic;

/// The tun's trust-on-first-use pin store (issue #153).
pub mod pin;

/// Which transport carries a tunnel's byte streams.
///
/// Two exist after issue #153: QUIC (the primary transport) and
/// WebSocket-over-TCP (the fallback for networks where UDP is
/// blocked). The name is used in the logs.
///
/// The tun's *selector* (`auto` / `quic` / `tcp`) is a separate type
/// — `crates/tun/src/config.rs::TransportSelector` — because it also
/// carries the `auto` value, which has no counterpart here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransportKind {
    /// QUIC — one connection per tun, one bidirectional stream per
    /// request. Encrypted by default.
    Quic,
    /// WebSocket over TCP — the pre-#153 transport, kept as the
    /// fallback.
    Tcp,
}

impl TransportKind {
    /// The stable, lower-case name used in configuration, logs and
    /// the admin UI.
    pub fn as_str(&self) -> &'static str {
        match self {
            TransportKind::Quic => "quic",
            TransportKind::Tcp => "tcp",
        }
    }

    /// Parse the name produced by [`TransportKind::as_str`].
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "quic" => Some(TransportKind::Quic),
            "tcp" | "websocket" | "ws" => Some(TransportKind::Tcp),
            _ => None,
        }
    }
}

impl std::fmt::Display for TransportKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A byte stream carried by a tunnel transport.
///
/// The tunnel is a **splice** (issue #153): the gateway writes HTTP
/// into a tunnel stream and the tun copies those bytes to the backend
/// socket without interpreting them. Both transports therefore hand
/// out plain byte streams — nothing above this seam knows whether the
/// bytes travel over a QUIC bidirectional stream or a yamux stream
/// inside a WebSocket.
pub trait TunnelByteStream: AsyncRead + AsyncWrite + Unpin + Send + Sync {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + Sync> TunnelByteStream for T {}

/// A boxed tunnel byte stream.
pub type TunnelStream = Box<dyn TunnelByteStream>;

/// A transport that carries a tun's byte streams.
///
/// The gateway uses [`TunnelTransport::open_stream`] to start a new
/// request stream towards a tun; the tun uses
/// [`TunnelTransport::accept_stream`] to pick up the streams the
/// gateway opened. Both directions exist on both transports (yamux and
/// QUIC are symmetric), so a single trait covers the two roles.
#[async_trait::async_trait]
pub trait TunnelTransport: Send + Sync {
    /// Open a new byte stream towards the tun.
    async fn open_stream(&self) -> crate::Result<TunnelStream>;

    /// Accept the next inbound byte stream from the gateway. `None`
    /// means the session has ended.
    async fn accept_stream(&self) -> Option<crate::Result<TunnelStream>>;

    /// Which transport this is — for the logs and the admin UI.
    fn kind(&self) -> TransportKind;
}

#[async_trait::async_trait]
impl TunnelTransport for YamuxTunnel {
    async fn open_stream(&self) -> crate::Result<TunnelStream> {
        // `YamuxTunnel::open_stream` is the inherent method; call it
        // explicitly so the trait method does not shadow it.
        let stream = YamuxTunnel::open_stream(self).await?;
        Ok(Box::new(stream))
    }

    async fn accept_stream(&self) -> Option<crate::Result<TunnelStream>> {
        match YamuxTunnel::accept_stream(self).await {
            Some(Ok(stream)) => Some(Ok(Box::new(stream))),
            Some(Err(e)) => Some(Err(crate::error::PangolinError::Io(e))),
            None => None,
        }
    }

    fn kind(&self) -> TransportKind {
        TransportKind::Tcp
    }
}

// ---------------------------------------------------------------------------
// Connection instruction
// ---------------------------------------------------------------------------

/// Encode the connection instruction the gateway writes at the start of
/// every tunnel stream:
///
/// ```text
/// CONNECT <host>:<port>\r\n
/// ```
///
/// The host is sent as a **name**, not as an address: the tun resolves
/// it in the operator's own network, so a backend that is only
/// reachable from the tun's side works. The instruction is a single
/// line, so the tun can read it before it has connected to anything.
pub fn encode_connect(host: &str, port: u16) -> Vec<u8> {
    // The instruction is a **line**: a host carrying CR, LF or NUL
    // would split it and leave a forged line in the tun's stream. The
    // backend URL is admin-controlled, so this is a defence in depth
    // rather than a reachable hole.
    if host.contains(['\r', '\n', '\0']) {
        // The host cannot be encoded safely, so the instruction is
        // deliberately malformed: the tun refuses it and the request
        // fails instead of smuggling a line.
        return b"CONNECT <invalid-host>\r\n".to_vec();
    }
    format!("CONNECT {host}:{port}\r\n").into_bytes()
}

/// Decode a `CONNECT <host>:<port>` instruction.
pub fn decode_connect(line: &[u8]) -> IoResult<(String, u16)> {
    let text = std::str::from_utf8(line)
        .map_err(|e| Error::new(ErrorKind::InvalidData, format!("connect line: utf-8: {e}")))?;
    let text = text.trim_end_matches(['\r', '\n']);
    let rest = text.strip_prefix("CONNECT ").ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidData,
            format!("expected a CONNECT instruction, got {text:?}"),
        )
    })?;
    let (host, port) = rest.rsplit_once(':').ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidData,
            format!("CONNECT instruction has no port: {text:?}"),
        )
    })?;
    if host.is_empty() {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!("CONNECT instruction has no host: {text:?}"),
        ));
    }
    let port: u16 = port.parse().map_err(|e| {
        Error::new(
            ErrorKind::InvalidData,
            format!("CONNECT instruction has a bad port: {text:?}: {e}"),
        )
    })?;
    // An IPv6 literal arrives bracketed (`[::1]:8080`). The brackets
    // are URL syntax, not part of the host — and `getaddrinfo` does
    // not accept them, so the tun would fail to resolve the
    // backend.
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    Ok((host.to_string(), port))
}

/// The tun's answer to a `CONNECT` instruction: `OK\r\n`, or
/// `ERR <reason>\r\n` when the target could not be reached.
pub fn encode_connect_reply(result: Result<(), &str>) -> Vec<u8> {
    match result {
        Ok(()) => b"OK\r\n".to_vec(),
        Err(reason) => format!("ERR {reason}\r\n").into_bytes(),
    }
}

/// Decode the tun's answer to a `CONNECT` instruction.
pub fn decode_connect_reply(line: &[u8]) -> IoResult<Result<(), String>> {
    let text = std::str::from_utf8(line)
        .map_err(|e| Error::new(ErrorKind::InvalidData, format!("reply: utf-8: {e}")))?;
    let text = text.trim_end_matches(['\r', '\n']);
    if text == "OK" {
        return Ok(Ok(()));
    }
    if let Some(reason) = text.strip_prefix("ERR ") {
        return Ok(Err(reason.to_string()));
    }
    Err(Error::new(
        ErrorKind::InvalidData,
        format!("unexpected CONNECT reply: {text:?}"),
    ))
}

// ---------------------------------------------------------------------------
// QUIC control-stream authentication
// ---------------------------------------------------------------------------

/// Encode the QUIC control line the tun writes right after the
/// handshake:
///
/// ```text
/// AUTH <token> <name> <protocol-version>\r\n
/// ```
///
/// QUIC has no upgrade request to carry the token in, so the tun opens
/// a control stream and authenticates there. The gateway validates the
/// token against the `tun` table **before** it opens any request stream
/// on that connection.
pub fn encode_auth(token: &str, name: &str, version: u32) -> Vec<u8> {
    format!("AUTH {token} {name} {version}\r\n").into_bytes()
}

/// Decode an `AUTH <token> <name> <version>` control line.
pub fn decode_auth(line: &[u8]) -> IoResult<(String, String, u32)> {
    let text = std::str::from_utf8(line)
        .map_err(|e| Error::new(ErrorKind::InvalidData, format!("auth line: utf-8: {e}")))?;
    let text = text.trim_end_matches(['\r', '\n']);
    let mut parts = text.split_whitespace();
    if parts.next() != Some("AUTH") {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!("expected an AUTH line, got {text:?}"),
        ));
    }
    let token = parts.next().ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidData,
            format!("AUTH line has no token: {text:?}"),
        )
    })?;
    let name = parts.next().ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidData,
            format!("AUTH line has no name: {text:?}"),
        )
    })?;
    let version = parts
        .next()
        .ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidData,
                format!("AUTH line has no protocol version: {text:?}"),
            )
        })?
        .parse::<u32>()
        .map_err(|e| {
            Error::new(
                ErrorKind::InvalidData,
                format!("AUTH line has a bad protocol version: {text:?}: {e}"),
            )
        })?;
    Ok((token.to_string(), name.to_string(), version))
}

/// The message both sides use when the protocol versions disagree.
///
/// It names **both** versions so the operator can tell which side is
/// behind, instead of guessing from a generic protocol error.
pub fn protocol_version_mismatch(ours: u32, theirs: u32) -> String {
    // ASCII only: the message travels in an HTTP status line (the
    // WebSocket/TCP transport refuses the upgrade with it), and a
    // status line has to stay ASCII.
    format!(
        "tunnel protocol version mismatch: this side speaks v{ours}, the peer speaks v{theirs} \
         - upgrade the tun and the gateway together"
    )
}

/// Encode the gateway's answer to an `AUTH` control line:
/// `OK <protocol-version>\r\n`, or `ERR <reason>\r\n`.
pub fn encode_auth_reply(result: Result<u32, &str>) -> Vec<u8> {
    match result {
        Ok(version) => format!("OK {version}\r\n").into_bytes(),
        Err(reason) => format!("ERR {reason}\r\n").into_bytes(),
    }
}

/// Decode the gateway's answer to an `AUTH` control line.
///
/// On success the answer carries the gateway's protocol version, so
/// the tun can refuse a gateway that speaks a different one even when
/// the tun's own line was accepted.
pub fn decode_auth_reply(line: &[u8]) -> IoResult<Result<u32, String>> {
    let text = std::str::from_utf8(line)
        .map_err(|e| Error::new(ErrorKind::InvalidData, format!("auth reply: utf-8: {e}")))?;
    let text = text.trim_end_matches(['\r', '\n']);
    if let Some(rest) = text.strip_prefix("OK") {
        let version = rest
            .trim()
            .parse::<u32>()
            .map_err(|e| Error::new(ErrorKind::InvalidData, format!("auth reply: {e}")))?;
        return Ok(Ok(version));
    }
    if let Some(reason) = text.strip_prefix("ERR ") {
        return Ok(Err(reason.to_string()));
    }
    Err(Error::new(
        ErrorKind::InvalidData,
        format!("unexpected AUTH reply: {text:?}"),
    ))
}

/// Read a single CRLF-terminated line off a byte stream.
///
/// Used for the `CONNECT` instruction, the `OK` / `ERR` reply and the
/// `AUTH` control line. The line is returned **without** its
/// terminator. `max` bounds the read so a peer that never sends a
/// newline cannot make us allocate without limit.
///
/// **The read is one byte at a time on purpose.** The gateway reads
/// the tun's `OK` reply off the *same* stream that later carries the
/// response, so a buffered read would swallow the first response
/// bytes. One byte at a time leaves the rest of the stream exactly
/// where it was.
pub async fn read_crlf_line<R>(r: &mut R, max: usize) -> IoResult<String>
where
    R: AsyncRead + Unpin,
{
    let mut buf = Vec::with_capacity(64);
    let mut tmp = [0u8; 1];
    loop {
        let n = r.read(&mut tmp).await?;
        if n == 0 {
            return Err(Error::new(
                ErrorKind::UnexpectedEof,
                "eof before the end of the line",
            ));
        }
        if tmp[0] == b'\n' {
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            return Ok(String::from_utf8_lossy(&buf).into_owned());
        }
        buf.push(tmp[0]);
        if buf.len() > max {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!("line longer than {max} bytes without a terminator"),
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// The relay
// ---------------------------------------------------------------------------

/// Copy bytes in both directions with **explicit half-close** handling.
///
/// When one direction reaches end-of-file, only the *write* half of
/// the other side is shut down; the other direction keeps running.
/// That is what makes a long-lived response survive the end of the
/// request: the tun shuts the backend socket's write side down when
/// the gateway's direction ends and keeps reading the response.
///
/// A plain "copy until both sides are done" would tear the whole
/// connection down as soon as one side finished — killing a response
/// whose request side had already ended.
pub async fn relay_bidirectional<A, B>(a: &mut A, b: &mut B) -> IoResult<()>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let (mut a_read, mut a_write) = tokio::io::split(a);
    let (mut b_read, mut b_write) = tokio::io::split(b);
    let mut buf_ab = vec![0u8; 16 * 1024];
    let mut buf_ba = vec![0u8; 16 * 1024];
    let mut a_done = false;
    let mut b_done = false;

    while !(a_done && b_done) {
        tokio::select! {
            r = a_read.read(&mut buf_ab), if !a_done => {
                match r {
                    Ok(0) => {
                        // The peer will not send anything more: shut
                        // down the *write* half only, so the other
                        // direction can still deliver a response.
                        let _ = b_write.shutdown().await;
                        a_done = true;
                    }
                    Ok(n) => {
                        if b_write.write_all(&buf_ab[..n]).await.is_err() {
                            // The other side is gone — tear both
                            // directions down, or the loop would
                            // spin on a failing write.
                            let _ = a_write.shutdown().await;
                            let _ = b_write.shutdown().await;
                            a_done = true;
                            b_done = true;
                        }
                    }
                    Err(_) => {
                        let _ = b_write.shutdown().await;
                        a_done = true;
                    }
                }
            }
            r = b_read.read(&mut buf_ba), if !b_done => {
                match r {
                    Ok(0) => {
                        let _ = a_write.shutdown().await;
                        b_done = true;
                    }
                    Ok(n) => {
                        if a_write.write_all(&buf_ba[..n]).await.is_err() {
                            let _ = b_write.shutdown().await;
                            let _ = a_write.shutdown().await;
                            a_done = true;
                            b_done = true;
                        }
                    }
                    Err(_) => {
                        let _ = a_write.shutdown().await;
                        b_done = true;
                    }
                }
            }
        }
    }
    Ok(())
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
                    // `session.next()` yields Err only for **session-level**
                    // fatals — today this is exclusively the yamux
                    // keepalive timeout (ping outstanding longer than
                    // `TIMEOUT` ≈ 30s; triggered by `kill -9`, OOM,
                    // container restart, network partition, etc.) and
                    // any transport-level I/O error on the underlying
                    // duplex (yamux surfaces transport errors through
                    // the same path). yamux's `Stream` impl does not
                    // surface per-stream errors here — those are
                    // observed by the `StreamHandle`'s own read/write
                    // methods, not by `Session::poll_next`.
                    //
                    // This Err must therefore terminate the driver
                    // loop. The pre-fix behaviour — forwarding it to a
                    // single pending acceptor and continuing — left
                    // `session.is_dead()` false, so the surrounding
                    // bridge task never woke up `session_end` and the
                    // `mark_tun_offline` call site never ran; the DB
                    // row stayed `online=1` until the OS TCP timeout
                    // (minutes to hours) finally caught the half-open
                    // connection. Breaking out lets the surrounding
                    // `tokio::select!` reach the
                    // `session_end_for_task.notify_waiters()` line so
                    // the existing call site fires.
                    //
                    // Pending acceptors are drained with EOF below the
                    // loop, so they don't hang.
                    Some(Err(e)) => {
                        // Surface the error at debug so operators can
                        // distinguish a clean peer disconnect (the
                        // `None` arm above) from a real session-level
                        // fatal — keepalive timeout (the half-open
                        // case the issue is fixing), transport I/O
                        // error, or go-away. Without this, every
                        // session end looks identical in the logs.
                        debug!("tunnel: session ended with error: {e}");
                        break;
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

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

// ---------------------------------------------------------------------------
// WS relay: hand-written half-close byte pump
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
///
/// The `Debug` impl redacts the `Authorization` header: the struct
/// carries the tun's bearer token, and a `{:?}` dump would put the
/// credential into the log.
#[derive(Clone)]
pub struct WsUpgradeRequest {
    pub method: String,
    pub target: String,
    pub version: String,
    pub authorization: Option<String>,
    pub sec_websocket_key: Option<String>,
    /// Every header of the request, in order. The tunnel protocol
    /// version travels in [`TUNNEL_VERSION_HEADER`]; the named fields
    /// above are the ones the handshake itself needs.
    pub headers: Vec<(String, String)>,
}

impl std::fmt::Debug for WsUpgradeRequest {
    /// Redacts the `Authorization` header.
    ///
    /// The struct carries the tun's bearer token; the derived `Debug`
    /// would put it into every log line that dumps the request.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redact = |name: &str| name.eq_ignore_ascii_case("authorization");
        f.debug_struct("WsUpgradeRequest")
            .field("method", &self.method)
            .field("target", &self.target)
            .field("version", &self.version)
            .field(
                "authorization",
                &self.authorization.as_ref().map(|_| "<redacted>"),
            )
            .field("sec_websocket_key", &self.sec_websocket_key)
            .field(
                "headers",
                &self
                    .headers
                    .iter()
                    .map(|(k, v)| {
                        if redact(k) {
                            (k, "<redacted>".to_string())
                        } else {
                            (k, v.clone())
                        }
                    })
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// The header the WebSocket/TCP transport carries the tunnel protocol
/// version in.
///
/// The WebSocket upgrade request is the only handshake the TCP
/// fallback has, so the version rides along as an extra header. A
/// tun that omits it is a tun that predates issue #153 — the gateway
/// refuses the upgrade rather than speaking two protocols at once.
pub const TUNNEL_VERSION_HEADER: &str = "X-Pangolin-Tunnel-Version";

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
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let k_lc = k.trim().to_ascii_lowercase();
            let v = v.trim().to_string();
            if k_lc == "authorization" {
                authorization = Some(v.clone());
            } else if k_lc == "sec-websocket-key" {
                sec_websocket_key = Some(v.clone());
            }
            headers.push((k.trim().to_string(), v));
        }
    }
    WsUpgradeRequest {
        method,
        target,
        version,
        authorization,
        sec_websocket_key,
        headers,
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
///
/// `extra_headers` are appended after the built-in ones. The tun's
/// own registration handshake (tun → gateway) carries no client
/// context and passes an empty slice, so its bytes are unchanged.
/// The tun's backend upgrade (gateway → backend) passes the four
/// `P-*` gateway headers plus the forwarded-for chain, which the
/// regular HTTP path forwards implicitly.
///
/// Header values are sanitized: CR / LF are stripped so a value
/// that reached us from the wire can never inject a header into the
/// backend's request.
pub fn build_ws_upgrade_request(
    path: &str,
    host: &str,
    token: &str,
    extra_headers: &[(String, String)],
) -> (Vec<u8>, String) {
    let key = generate_ws_key();
    let mut req = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         Authorization: Bearer {token}\r\n"
    );
    for (name, value) in extra_headers {
        req.push_str(name);
        req.push_str(": ");
        req.push_str(&sanitize_header_value(value));
        req.push_str("\r\n");
    }
    req.push_str("\r\n");
    (req.into_bytes(), key)
}

/// Strip CR / LF from a header value so it cannot terminate the
/// header line it is written into.
fn sanitize_header_value(value: &str) -> String {
    value.chars().filter(|c| *c != '\r' && *c != '\n').collect()
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

    // -------------------------------------------------------------------
    // Connection instruction (issue #153)
    // -------------------------------------------------------------------

    /// The `CONNECT` instruction is one line, so the tun can read it
    /// before it has connected to anything.
    #[test]
    fn connect_instruction_roundtrips() {
        let bytes = encode_connect("10.0.0.7", 9000);
        assert_eq!(bytes, b"CONNECT 10.0.0.7:9000\r\n");
        let (host, port) = decode_connect(&bytes).unwrap();
        assert_eq!(host, "10.0.0.7");
        assert_eq!(port, 9000);
    }

    /// The host travels as a **name**: the tun resolves it in the
    /// operator's own network, so a backend that is only reachable
    /// from the tun's side works.
    #[test]
    fn connect_instruction_carries_a_host_name() {
        let bytes = encode_connect("backend.internal", 8080);
        let (host, port) = decode_connect(&bytes).unwrap();
        assert_eq!(host, "backend.internal");
        assert_eq!(port, 8080);
    }

    /// An IPv6 literal arrives bracketed; the brackets are URL syntax
    /// and `getaddrinfo` does not accept them, so they must be
    /// stripped before the tun resolves the host.
    #[test]
    fn connect_instruction_strips_the_brackets_of_an_ipv6_literal() {
        let bytes = encode_connect("[::1]", 8080);
        let (host, port) = decode_connect(&bytes).unwrap();
        assert_eq!(host, "::1");
        assert_eq!(port, 8080);
    }

    /// A host carrying a line break would split the instruction and
    /// leave a forged line in the tun's stream, so it is refused.
    #[test]
    fn connect_instruction_refuses_a_host_with_a_line_break() {
        let bytes = encode_connect("evil\r\nCONNECT other:1", 80);
        // The instruction must stay a single line.
        assert_eq!(
            bytes.iter().filter(|b| **b == b'\n').count(),
            1,
            "the instruction must not contain a second line: {:?}",
            String::from_utf8_lossy(&bytes)
        );
        assert!(!String::from_utf8_lossy(&bytes).contains("evil"));
    }

    /// A `CONNECT` instruction without a port is refused — the tun
    /// would have nothing to dial.
    #[test]
    fn connect_instruction_without_port_is_refused() {
        let err = decode_connect(b"CONNECT 10.0.0.7\r\n").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    /// A line that is not a `CONNECT` at all is refused too.
    #[test]
    fn connect_instruction_with_wrong_verb_is_refused() {
        let err = decode_connect(b"GET / HTTP/1.1\r\n").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    /// The tun answers `OK` or `ERR <reason>`; both must survive the
    /// round-trip, and `ERR` must keep its reason.
    #[test]
    fn connect_reply_roundtrips() {
        assert_eq!(
            decode_connect_reply(&encode_connect_reply(Ok(()))).unwrap(),
            Ok(())
        );
        let err = decode_connect_reply(&encode_connect_reply(Err("no route to host"))).unwrap();
        assert_eq!(err, Err("no route to host".to_string()));
    }

    /// An unknown reply is a protocol error, not a silent success.
    #[test]
    fn connect_reply_with_unknown_verb_is_refused() {
        let err = decode_connect_reply(b"MAYBE\r\n").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    // -------------------------------------------------------------------
    // QUIC control-stream authentication (issue #153)
    // -------------------------------------------------------------------

    /// The `AUTH` line carries the token, the tun's name and the
    /// protocol version.
    #[test]
    fn auth_line_roundtrips() {
        let bytes = encode_auth("secret-token", "office", TUNNEL_PROTOCOL_VERSION);
        let (token, name, version) = decode_auth(&bytes).unwrap();
        assert_eq!(token, "secret-token");
        assert_eq!(name, "office");
        assert_eq!(version, TUNNEL_PROTOCOL_VERSION);
    }

    /// An `AUTH` line without a protocol version is refused: the
    /// gateway could not tell whether the peer speaks the same wire
    /// format.
    #[test]
    fn auth_line_without_version_is_refused() {
        let err = decode_auth(b"AUTH token office\r\n").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    /// A version mismatch must name **both** versions, so the operator
    /// can tell which side is behind.
    #[test]
    fn protocol_version_mismatch_names_both_versions() {
        let msg = protocol_version_mismatch(TUNNEL_PROTOCOL_VERSION, 1);
        assert!(
            msg.contains(&format!("v{}", TUNNEL_PROTOCOL_VERSION)),
            "{msg}"
        );
        assert!(msg.contains("v1"), "{msg}");
    }

    // -------------------------------------------------------------------
    // Transport kind
    // -------------------------------------------------------------------

    /// The transport names round-trip through the parser — the tun's
    /// `transport:` selector and the log lines use the same spelling.
    #[test]
    fn transport_kind_names_roundtrip() {
        for kind in [TransportKind::Quic, TransportKind::Tcp] {
            assert_eq!(TransportKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(TransportKind::parse("QUIC"), Some(TransportKind::Quic));
        assert_eq!(TransportKind::parse(" tcp "), Some(TransportKind::Tcp));
        assert_eq!(TransportKind::parse("carrier-pigeon"), None);
    }

    // -------------------------------------------------------------------
    // The relay
    // -------------------------------------------------------------------

    /// The relay must keep the response direction open after the
    /// request direction has ended — that is the whole point of the
    /// half-close handling. A backend that answers only after the
    /// request is finished has to reach the client.
    #[tokio::test]
    async fn relay_keeps_the_response_direction_open_after_the_request_ends() {
        let (mut gateway, mut backend) = tokio::io::duplex(1024);

        // The backend only answers once it has seen the end of the
        // request — i.e. after the gateway half-closed.
        let backend_task = tokio::spawn(async move {
            let mut buf = [0u8; 64];
            let n = backend.read(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], b"ping");
            // Wait for the half-close (EOF on the request side).
            let mut rest = Vec::new();
            backend.read_to_end(&mut rest).await.unwrap();
            assert!(rest.is_empty());
            backend.write_all(b"pong").await.unwrap();
            backend.shutdown().await.unwrap();
        });

        // The gateway writes the request and then closes its write
        // side; the response must still arrive.
        gateway.write_all(b"ping").await.unwrap();
        gateway.shutdown().await.unwrap();

        let mut seen = Vec::new();
        let mut buf = [0u8; 16];
        loop {
            let n = gateway.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            seen.extend_from_slice(&buf[..n]);
        }
        assert_eq!(seen, b"pong");
        backend_task.await.unwrap();
    }

    /// The relay must stop when both directions are done — it must not
    /// spin forever after the backend closed.
    #[tokio::test]
    async fn relay_returns_when_both_directions_end() {
        let (mut a, mut a_peer) = tokio::io::duplex(64);
        let (mut b, mut b_peer) = tokio::io::duplex(64);
        let backend = tokio::spawn(async move {
            b_peer.write_all(b"hello").await.unwrap();
            b_peer.shutdown().await.unwrap();
        });
        // The gateway side closes its write half, so the relay's first
        // direction ends as well and the relay can finish.
        a_peer.shutdown().await.unwrap();
        relay_bidirectional(&mut a, &mut b).await.unwrap();
        backend.await.unwrap();
        let mut seen = Vec::new();
        a_peer.read_to_end(&mut seen).await.unwrap();
        assert_eq!(seen, b"hello");
    }

    // -------------------------------------------------------------------
    // session_driver regression: session_end must fire when the
    // underlying transport ends. Issue #68 fixes a bug where the
    // pre-fix driver forwarded `session.next()` Err to a pending
    // acceptor and continued, leaving `session_end` unfired (so
    // `mark_tun_offline` was never called and the DB row stayed
    // online). The test below exercises the **clean-disconnect EOF
    // path** end-to-end to make sure the driver still surfaces a
    // session-end notification after the fix. The half-open Err
    // path (yamux keepalive timeout, ~30s) is covered by manual
    // repro because yamux hardcodes the timeout (see
    // `tokio-yamux-*/src/session.rs` const `TIMEOUT = 30s`) and
    // would push the test into the minute-range.
    // -------------------------------------------------------------------
    use tokio::io::DuplexStream;

    /// Build a `YamuxTunnel` over an in-memory `tokio::io::duplex`
    /// pair, spawn a peer task that just reads forever, and
    /// block until the driver is plausibly in its `select!` loop.
    ///
    /// The returned `peer_handle` is the only thing keeping the
    /// peer half of the duplex open. Aborting it (or letting it
    /// drop) closes the peer's end of the pipe, which surfaces
    /// to `ws_bridge` as an EOF and from there to `session_driver`
    /// as `session.next() -> None` (clean disconnect path).
    ///
    /// The 50ms sleep is a soft synchronization point: with
    /// tokio's current_thread test runtime the bridge task is
    /// spawned and polled during the sleep, so by the time this
    /// returns the driver has almost certainly entered its loop.
    /// A future tightening could replace the sleep with an
    /// explicit readiness signal from the driver, at the cost
    /// of a new public test API on `YamuxTunnel`.
    async fn make_test_tunnel() -> (YamuxTunnel, tokio::task::JoinHandle<()>) {
        let (peer, server_io) = tokio::io::duplex(64 * 1024);
        let server_ws = fastwebsockets::WebSocket::after_handshake(server_io, WsRole::Server);
        let tunnel = tunnel_over_websocket(server_ws, TunnelRole::Server);

        let peer_handle = tokio::spawn(async move {
            let mut peer: DuplexStream = peer;
            let mut buf = [0u8; 1024];
            while peer.read(&mut buf).await.is_ok() {}
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        (tunnel, peer_handle)
    }

    /// Simulate a clean peer-side disconnect: abort the peer
    /// task (drops `peer` at the await point) and wait for the
    /// JoinHandle to finalise. Mirrors what `kill -9` does to a
    /// real tun process — the OS reaps the fd, no FIN/RST.
    async fn drop_peer(peer_handle: tokio::task::JoinHandle<()>) {
        peer_handle.abort();
        let _ = peer_handle.await;
    }

    /// A clean remote-side close (the peer drops its WS) must reach
    /// `session_end` on this side. Without the fix, the WS-bridge
    /// EOF path was already correct; this test guards against
    /// regressing it while we add Err handling for the half-open
    /// case.
    #[tokio::test]
    async fn session_end_fires_when_peer_closes_ws() {
        let (tunnel, peer_handle) = make_test_tunnel().await;

        // Register a waiter on session_end **before** triggering
        // the disconnect. tokio::sync::Notify::notify_waiters
        // only wakes currently-registered waiters, so a
        // post-disconnect `notified().await` would race against
        // the bridge finishing first and miss the signal. The
        // production call site (crates/ngx/src/tunnel.rs) does
        // the same thing — call .notified() before any chance
        // of notification — so this is a faithful test of the
        // real pattern.
        let end = tunnel.session_end.clone();
        let waiter = tokio::spawn(async move { end.notified().await });

        drop_peer(peer_handle).await;

        match tokio::time::timeout(std::time::Duration::from_secs(5), waiter).await {
            Ok(Ok(())) => {} // good — session_end fired
            Ok(Err(e)) => panic!("session_end waiter join errored: {e}"),
            Err(_) => panic!(
                "session_end did not fire within 5s after peer disconnect — \
                 mark_tun_offline would never run and the DB row would stay online"
            ),
        }
    }

    /// An open_stream call issued AFTER session_end fired must
    /// fail cleanly with BrokenPipe (not hang forever, not leak
    /// the open_rx sender). The post-loop drain on
    /// `pending_accepts` only covers acceptors; this guards the
    /// `open_tx` path which is dropped when session_driver
    /// returns.
    #[tokio::test]
    async fn open_stream_after_session_end_fails_cleanly() {
        let (tunnel, peer_handle) = make_test_tunnel().await;

        drop_peer(peer_handle).await;

        let end = tunnel.session_end.clone();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), end.notified()).await;

        // open_stream must now fail — the open_tx receiver has
        // been dropped with session_driver.
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(2), tunnel.open_stream()).await;
        match result {
            Ok(Err(e)) => assert_eq!(e.kind(), ErrorKind::BrokenPipe),
            Ok(Ok(_)) => panic!("open_stream unexpectedly succeeded after session end"),
            Err(_) => panic!("open_stream hung after session end"),
        }
    }

    /// Issue #68 verification checklist step 7 — the
    /// "concurrent open-accept test" — guards against two
    /// leak risks in `accept_stream` after the session ends:
    ///
    /// (a) A caller that called `accept_stream()` *before* the
    ///     peer disconnected must not hang forever. The driver
    ///     must wake the pending acceptor either by running
    ///     the post-loop drain (sending `UnexpectedEof("yamux
    ///     session ended")`) or by dropping `pending_accepts`
    ///     (which drops the oneshot sender, making the
    ///     receiver return `Err` and `accept_stream` return
    ///     `None`). Both outcomes are acceptable; a hang is not.
    ///
    /// (b) A caller that calls `accept_stream()` *after*
    ///     session_end must observe `None` promptly (the driver
    ///     has dropped `accept_rx`, so `accept_tx.send` fails
    ///     and the impl short-circuits to `None` without
    ///     awaiting the oneshot).
    ///
    /// Note on the current `tunnel_over_websocket` layout: the
    /// driver and `ws_bridge` share a single `tokio::spawn`'d
    /// task wrapped in `tokio::select!`. When the peer EOFs the
    /// WS, `ws_bridge` finishes first and the driver future is
    /// *cancelled*, so its post-loop drain never runs — case
    /// (a) takes the second path (`None` via dropped
    /// `pending_accepts`). This is a pre-existing structural
    /// quirk, not a #68 regression. The #68 fix
    /// (`Some(Err(_e)) => break`) only changes behaviour for
    /// session-level fatals surfaced via `session.next()`; it
    /// does not change the clean-EOF path. Restructuring the
    /// bridge/driver tasks to let the drain always run is out
    /// of scope for #68.
    #[tokio::test]
    async fn accept_stream_does_not_hang_on_peer_disconnect() {
        let (tunnel, peer_handle) = make_test_tunnel().await;

        // --- Case (a): queue an accept BEFORE the disconnect. ---
        let tunnel_for_accept = tunnel.clone();
        let queued = tokio::spawn(async move { tunnel_for_accept.accept_stream().await });

        // Register a session_end waiter BEFORE the peer drops.
        // tokio::sync::Notify::notify_waiters only wakes already-
        // registered waiters; a post-disconnect `.notified().await`
        // would race against the bridge finishing first and miss
        // the signal. The production call site in
        // crates/ngx/src/tunnel.rs does the same thing.
        let end = tunnel.session_end.clone();
        let end_waiter = tokio::spawn(async move { end.notified().await });

        drop_peer(peer_handle).await;

        // Sanity: session_end must fire within a few seconds.
        tokio::time::timeout(std::time::Duration::from_secs(5), end_waiter)
            .await
            .expect("session_end_waiter timed out")
            .expect("session_end_waiter task panicked");

        // The queued accept must complete (no hang). The exact
        // value is environment-dependent on the bridge-task
        // layout — see the function-level comment — so accept
        // either None (driver cancelled, pending_accepts
        // dropped) or Some(Err(UnexpectedEof)) (post-loop drain
        // ran). Some(Ok(_)) would be wrong (peer is gone).
        match tokio::time::timeout(std::time::Duration::from_secs(5), queued).await {
            // JoinHandle outer Ok, inner is Some(Ok(_)) — a live
            // StreamHandle came back even though the peer is gone.
            Ok(Ok(Some(Ok(_)))) => panic!(
                "queued accept_stream returned a real StreamHandle after the peer \
                 disconnected — accept should never resolve with a live stream once \
                 the session is gone"
            ),
            // JoinHandle outer Ok, inner is Some(Err(_)) (post-loop
            // drain fired) or None (driver cancelled, accept_rx
            // dropped) — both are clean failures, accept either.
            Ok(Ok(Some(Err(_)))) | Ok(Ok(None)) => {}
            // JoinError: the task itself panicked.
            Ok(Err(e)) => panic!("queued accept_stream task panicked: {e}"),
            // Timeout: hung past 5s. This is the actual regression
            // we are guarding against.
            Err(_) => panic!(
                "queued accept_stream hung past 5s after peer disconnect — \
                 pending_accepts queue leaked (issue #68 verification step 7)"
            ),
        }

        // --- Case (b): a new accept after session_end. ---
        // The driver has dropped accept_rx, so accept_tx.send
        // returns Err and accept_stream short-circuits to None.
        match tokio::time::timeout(std::time::Duration::from_secs(2), tunnel.accept_stream()).await
        {
            Ok(None) => {} // good — fast-path None, no hang
            Ok(Some(_)) => panic!(
                "accept_stream after session_end returned Some(_) — driver should \
                 have dropped accept_rx"
            ),
            Err(_) => panic!("accept_stream after session_end hung past 2s"),
        }
    }
}
