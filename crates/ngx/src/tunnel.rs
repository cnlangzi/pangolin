//! Tunnel endpoint — receives connections from remote tun nodes.
//!
//! ## Protocol (issue #153 — the tunnel is a splice)
//!
//! The tunnel carries **raw bytes**. The gateway's pingora writes the
//! client's request into a tunnel stream; the tun copies those bytes
//! to the backend socket and copies the backend's bytes back. The
//! tun never speaks HTTP.
//!
//! Two transports implement the same byte-stream interface
//! (`pangolin_tun::TunnelTransport`):
//!
//! * **QUIC** (primary) — one connection per tun, one bidirectional
//!   stream per request. Encrypted by default. The tun authenticates
//!   on a control stream right after the handshake.
//! * **WebSocket over TCP** (fallback) — the pre-#153 transport, kept
//!   for networks where UDP is blocked. The token and the protocol
//!   version ride in the WebSocket upgrade request.
//!
//! Both transports carry the tunnel protocol version, so a tun and a
//! gateway that were not upgraded together fail with a clear message
//! instead of a protocol error mid-request.
//!
//! ## The WebSocket/TCP handshake
//!
//! 1. tun opens a TCP connection to ngx's tunnel listener.
//! 2. tun sends a `GET /tunnel?name=<NAME> HTTP/1.1` upgrade request
//!    with `Authorization: Bearer <token>` (the token's sha256 is
//!    what's stored in the DB; we hash the inbound bearer before
//!    lookup) and the protocol version in the
//!    `X-Pangolin-Tunnel-Version` header.
//! 3. ngx validates the bearer via the `tun` table and the protocol
//!    version. On success: send `101 Switching Protocols`. On failure:
//!    send a real HTTP 400/401/403/500 and close.
//! 4. Once the WS is up, a yamux server session is layered on top.
//!    Each request from ngx's proxy path opens one yamux stream
//!    carrying the `CONNECT` instruction and then raw bytes.
//! 5. The session is registered in `App::tun_sessions` keyed by
//!    `tun_name`. When a tun disconnects, the entry is removed.

pub mod connector;
pub mod quic;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use fastwebsockets::{Role as WsRole, WebSocket};
use log::{debug, error, info, warn};

use pangolin_tun::{
    TUNNEL_PROTOCOL_VERSION, TUNNEL_VERSION_HEADER, TunnelRole, TunnelTransport, bearer_token,
    protocol_version_mismatch, read_ws_upgrade_request, tunnel_over_websocket, write_http_error,
    write_ws_accept_response,
};

use crate::App;

// ---- the drain ----

/// How long the drain waits for the in-flight tunnel streams.
///
/// The second stage of the drain (issue #234): the listeners have
/// stopped accepting, and now the tunnel sessions are closed so the
/// tun reconnects — to the incoming generation, which is already
/// serving. The budget is the wait for the streams that are still
/// running: a session closed under a live request would fail it.
///
/// The budget is capped by the configuration's drain timeout: the
/// whole drain has to fit into it.
pub(crate) const TUNNEL_DRAIN_BUDGET: Duration = Duration::from_secs(5);

/// How a live tunnel session is closed when the generation drains.
///
/// The transport trait has no `close`, and the two transports close
/// differently: a QUIC session needs a CONNECTION_CLOSE frame, a
/// WebSocket session needs its bridge task aborted. The gateway
/// therefore registers a closer per session and calls it when the
/// generation drains.
pub(crate) type SessionCloser = Box<dyn Fn() + Send + Sync>;

/// The live tunnel sessions, with the closer that shuts one down.
///
/// The key is the tun's name and the value carries the session's
/// identity: a tun that reconnects registers a **new** closer under
/// the same name, and the old session's cleanup must not take the
/// new one away with it.
static SESSIONS: LazyLock<Mutex<HashMap<String, (SessionId, SessionCloser)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The identity of a registered tunnel session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SessionId(u64);

/// The next session identity.
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

/// The tunnel streams that are open right now.
///
/// The drain waits for the count to reach zero before it closes the
/// sessions — a session closed under a live request would fail that
/// request, and the handover is supposed to be invisible.
static OPEN_STREAMS: AtomicUsize = AtomicUsize::new(0);

/// Register a session's closer and return its identity.
pub(crate) fn register_session(name: &str, closer: SessionCloser) -> SessionId {
    let id = SessionId(NEXT_SESSION_ID.fetch_add(1, Ordering::SeqCst));
    SESSIONS
        .lock()
        .expect("the session registry")
        .insert(name.to_string(), (id, closer));
    id
}

/// Forget a session that ended on its own.
///
/// The identity is checked: when a tun reconnects, the **new**
/// session's closer is under the same name, and the old session's
/// cleanup must not take it away — the drain would then leave that
/// live session open, and the tun would stay bound to the outgoing
/// generation.
pub(crate) fn unregister_session(name: &str, id: SessionId) {
    let mut sessions = SESSIONS.lock().expect("the session registry");
    if let Some((current, _)) = sessions.get(name)
        && *current == id
    {
        sessions.remove(name);
    }
}

/// The number of tunnel streams that are open right now.
pub(crate) fn open_streams() -> usize {
    OPEN_STREAMS.load(Ordering::SeqCst)
}

/// A guard that counts one open tunnel stream.
///
/// The guard lives as long as the bridge that carries the stream, so
/// the count is the number of requests that are still travelling
/// through the tunnel.
pub(crate) struct StreamGuard;

impl StreamGuard {
    /// Count one more open stream.
    pub(crate) fn new() -> Self {
        OPEN_STREAMS.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        OPEN_STREAMS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Close the tunnel sessions — the second stage of the drain.
///
/// The in-flight streams get their budget first: a session closed
/// under a live request would fail that request, and the handover is
/// supposed to be invisible. A stream that is still running when the
/// budget runs out is lost — the alternative is holding the whole
/// drain open for a request that never ends.
///
/// Afterwards the tun reconnects, and the reconnect lands on the
/// incoming generation: the outgoing one has stopped accepting.
pub(crate) async fn close_sessions_for_drain(budget: Duration) {
    let deadline = Instant::now() + budget;
    while open_streams() > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let closers: Vec<SessionCloser> = {
        let mut sessions = SESSIONS.lock().expect("the session registry");
        sessions.drain().map(|(_, (_, closer))| closer).collect()
    };
    let count = closers.len();
    for closer in closers {
        closer();
    }
    // The QUIC endpoint goes with the sessions. It has to stay alive
    // until here — the established sessions live on its UDP socket —
    // but it must not outlive them: a socket that stays in the
    // `SO_REUSEPORT` group keeps completing new handshakes that
    // nobody answers, and a tun that reconnects into it waits for
    // the AUTH reply until its own timeout runs out.
    quic::close_endpoint();
    info!("tunnel: closed {count} tunnel session(s) — the tun reconnects to the new generation");
}

// ---- Auth ----

/// Validate a tun's (name, token) against the `tun` table.
///
/// V3: `auth_tun` hashes the inbound token and compares
/// against `tun.token_hash`. We hash *here* in the call
/// path; the DB lookup is just the equality check.
pub(crate) async fn validate_token(app: &App, token: &str, tun_name: &str) -> Result<(), u16> {
    // The lookup runs on the blocking pool: it is a synchronous
    // SQLite call, and a worker thread that runs one is a worker
    // thread that is not proxying requests (issue #254).
    let token = token.to_string();
    let tun_name = tun_name.to_string();
    let row = match app
        .with_db(move |conn| pangolin_core::db::auth_tun(conn, &tun_name, &token))
        .await
    {
        Ok(Ok(row)) => row,
        // A refused read and a panicked read are the same answer for
        // the client: the gateway could not check the token.
        Ok(Err(_)) | Err(_) => return Err(500),
    };
    let now = chrono::Utc::now();
    match row {
        None => Err(401),
        Some((false, _)) => Err(403),
        Some((true, None)) => Ok(()),
        Some((true, Some(expires_at))) if expires_at > now => Ok(()),
        Some((true, Some(_))) => Err(401),
    }
}

pub(crate) async fn mark_tun_online(app: &App, tun_name: &str) {
    let tun_name = tun_name.to_string();
    let _ = app
        .with_db(move |conn| pangolin_core::db::set_tun_online(conn, &tun_name, true))
        .await;
}

pub(crate) async fn mark_tun_offline(app: &App, tun_name: &str) {
    let tun_name = tun_name.to_string();
    let _ = app
        .with_db(move |conn| pangolin_core::db::set_tun_online(conn, &tun_name, false))
        .await;
}

// ---- Main tunnel server ----

pub async fn start_tunnel_server(
    app: Arc<App>,
    addr: &str,
    shutdown: tokio_util::sync::CancellationToken,
) -> anyhow::Result<()> {
    let addr: SocketAddr = addr
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid tunnel server address {addr:?}: {e}"))?;

    // The listener asks for `SO_REUSEPORT` (issue #233): the socket
    // is built and bound here rather than by tokio's `bind`, because
    // the option has to be set **before** the bind. See
    // `crate::reuseport`.
    let listener = crate::reuseport::bind_tcp(addr)
        .map_err(|e| anyhow::anyhow!("failed to bind tunnel server on {addr}: {e}"))?;

    info!("tunnel server listening on {}", addr);

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                info!("tunnel: shutdown requested, stopping accept loop");
                return Ok(());
            }
            accept = listener.accept() => {
                match accept {
                    Ok((tcp_stream, client_addr)) => {
                        let app = app.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_client(app, tcp_stream, client_addr).await {
                                warn!("client {} error: {}", client_addr, e);
                            }
                        });
                    }
                    Err(e) => {
                        error!("accept error: {}", e);
                    }
                }
            }
        }
    }
}

pub struct TunnelService {
    addr: String,
}

impl TunnelService {
    pub fn new(addr: impl Into<String>) -> Self {
        Self { addr: addr.into() }
    }
}

#[async_trait::async_trait]
impl crate::runtime::Service for TunnelService {
    fn name(&self) -> &'static str {
        "tunnel"
    }

    async fn run(&self, ctx: crate::runtime::ServiceContext) -> anyhow::Result<()> {
        start_tunnel_server(ctx.app, &self.addr, ctx.shutdown).await
    }
}

/// Handle a single tun client: WebSocket upgrade, auth,
/// then register the yamux session in `App::tun_sessions`.
async fn handle_client(
    app: Arc<App>,
    mut tcp_stream: tokio::net::TcpStream,
    _client_addr: SocketAddr,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Read the WS upgrade request and validate it.
    let req = read_ws_upgrade_request(&mut tcp_stream).await?;
    let token = match bearer_token(req.authorization.as_deref()) {
        Some(t) => t,
        None => {
            write_http_error(&mut tcp_stream, 401, "Unauthorized").await?;
            return Ok(());
        }
    };
    info!(
        "tunnel handshake: target={} token.len={}",
        req.target,
        token.len()
    );
    // Pull the tun name from the path. The tun client
    // dials `ws://host/tunnel` (no name in the path any
    // more — name comes from the `Authorization: Bearer
    // …:name=<NAME>` scheme). For backwards compat with
    // the old "?name=" style we'll also accept a query
    // parameter if the path is `/tunnel?name=…`.
    let name_from_query = req
        .target
        .split('?')
        .nth(1)
        .and_then(|q| {
            q.split('&')
                .find_map(|p| p.strip_prefix("name=").map(|s| s.to_string()))
        })
        .unwrap_or_default();
    let tun_name = if !name_from_query.is_empty() {
        name_from_query
    } else {
        // New style: `Authorization: Bearer <token>` was the
        // old way. The new tun-side writes
        // `Authorization: Bearer <token>` only — the name
        // comes from a separate, internal query parameter
        // pre-set in the Authorization header? No, the spec
        // actually says name comes from the path or query
        // string. We added a `?name=` convention for the
        // tun-side write in `build_ws_upgrade_request`. For
        // now, treat any tun that does NOT supply `?name=`
        // as a hard reject (we can't authorise without a
        // name).
        write_http_error(&mut tcp_stream, 400, "Bad Request: missing name").await?;
        return Ok(());
    };

    if !req.target.starts_with("/tunnel") {
        debug!("tunnel: wrong path, closing");
        write_http_error(&mut tcp_stream, 404, "Not Found").await?;
        return Ok(());
    }

    // The protocol version rides in the upgrade request. A tun that
    // omits it predates issue #153 and speaks the frame protocol,
    // which this gateway no longer understands — refuse it here with
    // a message that names both versions instead of failing later
    // inside the protocol.
    let peer_version = req
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(TUNNEL_VERSION_HEADER))
        .and_then(|(_, v)| v.trim().parse::<u32>().ok());
    if let Err(msg) = check_protocol_version(peer_version) {
        warn!("tunnel {}: {}", tun_name, msg);
        write_http_error(&mut tcp_stream, 400, &msg).await?;
        return Ok(());
    }

    let sec_key = match req.sec_websocket_key.as_deref() {
        Some(k) => k.to_string(),
        None => {
            write_http_error(
                &mut tcp_stream,
                400,
                "Bad Request: missing Sec-WebSocket-Key",
            )
            .await?;
            return Ok(());
        }
    };

    match validate_token(&app, &token, &tun_name).await {
        Ok(()) => {}
        Err(status) => {
            warn!("tunnel auth failed for {}: status {}", tun_name, status);
            let reason = match status {
                401 => "Unauthorized",
                403 => "Forbidden",
                _ => "Internal Server Error",
            };
            write_http_error(&mut tcp_stream, status, reason).await?;
            return Ok(());
        }
    }

    // Now upgrade the connection to a WebSocket. We can't
    // use fastwebsockets' accept_hdr_async (callback is
    // sync; we already validated the token above so we
    // don't need a second auth step).
    write_ws_accept_response(&mut tcp_stream, &sec_key).await?;

    // Wrap the TCP stream as a WebSocket and hand it to a
    // YamuxTunnel. The TCP stream is the underlying byte
    // pipe; the WebSocket frames yamux's bytes into
    // permessage-deflate-compressed binary frames.
    let ws = WebSocket::after_handshake(tcp_stream, WsRole::Server);
    let tunnel = tunnel_over_websocket(ws, TunnelRole::Server);

    // The closer the drain uses to shut this session down. The
    // bridge task owns the WebSocket, and aborting it drops the
    // socket — the tun sees the end of the connection and
    // reconnects.
    let closer: SessionCloser = Box::new({
        let abort = tunnel.bridge_abort.clone();
        move || abort.abort()
    });

    // Register the session keyed by tun_name. Reject
    // duplicates to avoid two tuns fighting for the same
    // routing slot.
    let session_end = tunnel.session_end.clone();
    let transport: Arc<dyn TunnelTransport> = Arc::new(tunnel);
    if !app.register_tun(tun_name.clone(), transport.clone()).await {
        warn!("tunnel: duplicate tun_name '{}' rejected", tun_name);
        return Ok(());
    }

    mark_tun_online(&app, &tun_name).await;
    let session_id = register_session(&tun_name, closer);
    // The transport is named in the log so the operator can tell a
    // QUIC session from a fallback session that silently happened.
    info!(
        "tun {} connected over the WebSocket/TCP transport",
        tun_name
    );
    app.add_event(pangolin_core::EventType::TunConnected {
        name: tun_name.clone(),
    });

    // Block on the bridge's end-of-session notification.
    // This future resolves when the WebSocket EOFs (the
    // tun dropped the connection). Until then, this
    // per-conn task stays alive and the registry entry
    // is reachable from the proxy hot path.
    session_end.notified().await;

    // Cleanup on disconnect — but only if this session is still the
    // registered one. A tun that reconnected (possibly on the other
    // transport) must not be unregistered by the old session's
    // cleanup.
    unregister_session(&tun_name, session_id);
    if app.unregister_tun_if(&tun_name, &transport).await {
        mark_tun_offline(&app, &tun_name).await;
        info!("tun {} disconnected", tun_name);
        app.add_event(pangolin_core::EventType::TunDisconnected {
            name: tun_name.clone(),
        });
    }
    Ok(())
}

/// Check the tun's protocol version against the gateway's.
///
/// A tun that reports a different version speaks a different wire
/// format, so the upgrade is refused with a message that names both
/// versions. A tun that reports **no** version predates issue #153
/// and speaks the frame protocol — also refused.
fn check_protocol_version(peer_version: Option<u32>) -> Result<(), String> {
    match peer_version {
        Some(v) if v == TUNNEL_PROTOCOL_VERSION => Ok(()),
        Some(v) => Err(protocol_version_mismatch(TUNNEL_PROTOCOL_VERSION, v)),
        None => Err(format!(
            "{} (the tun sent no {} header)",
            protocol_version_mismatch(TUNNEL_PROTOCOL_VERSION, 0),
            TUNNEL_VERSION_HEADER
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tun that speaks the gateway's protocol version is accepted.
    #[test]
    fn a_matching_protocol_version_is_accepted() {
        assert!(check_protocol_version(Some(TUNNEL_PROTOCOL_VERSION)).is_ok());
    }

    /// A tun that speaks a different version is refused, and the
    /// refusal names **both** versions.
    #[test]
    fn a_mismatched_protocol_version_is_refused_with_both_versions() {
        let err = check_protocol_version(Some(TUNNEL_PROTOCOL_VERSION + 1)).unwrap_err();
        assert!(
            err.contains(&format!("v{}", TUNNEL_PROTOCOL_VERSION)),
            "{err}"
        );
        assert!(
            err.contains(&format!("v{}", TUNNEL_PROTOCOL_VERSION + 1)),
            "{err}"
        );
    }

    /// A tun that sends no version header predates issue #153 and
    /// speaks the frame protocol — it is refused too.
    #[test]
    fn a_missing_protocol_version_is_refused() {
        let err = check_protocol_version(None).unwrap_err();
        assert!(err.contains(TUNNEL_VERSION_HEADER), "{err}");
    }
}
