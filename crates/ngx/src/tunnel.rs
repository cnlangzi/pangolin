//! Tunnel WebSocket endpoint — receives connections from remote tun nodes.
//!
//! The tun node connects via WS to `ngx:tunnel_port` with token/name in query.
//! After validating, the session is registered in `App.tun_sessions[tun_name]`.
//!
//! Flow:
//!   tun connects → WS handshake → validates token+name → marks online
//!   Proxy sends requests via App.tun_sessions → write_task reads from mpsc → sends ResponseFrame
//!   ngx reads request frames from WS (for future tun-initiated requests)
//!
//! Transport: MessagePack binary frames (not JSON).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use log::{debug, error, info, warn};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::{accept_async, tungstenite};

use pangolin_core::compress::{deflate_decode, deflate_encode};
use pangolin_core::{deserialize_msgpack, serialize_msgpack, TunnelFrame};

use crate::{App, TunnelMessage};

// ---- Auth ----

/// Validate a tun's (name, token) against the `tun` table.
///
/// v2: tokens and tun names live in the same row, so this is a single
/// SQL lookup. Three outcomes:
///
///   * `Ok(())` — a matching row exists, is `enabled=1`, and the
///     token has not expired (or has no expiry). The tun is admitted.
///   * `Err(401)` — either no row matches `(name, token)`, **or**
///     the row matches but its `expires_at` is in the past. Both
///     are hard rejects: a missing row means the (name, token) pair
///     must be admin-provisioned via the admin UI /tun/new form (POST /tun/new) before the tun
///     can come online (no auto-register), and an expired token
///     must be rotated by the admin. We collapse both into 401 so
///     the client doesn't learn "this (name, token) was once
///     valid, just expired" vs "never existed."
///   * `Err(403)` — a row matched and the token is not expired, but
///     `enabled=0` (admin disabled it). The tun cannot reconnect
///     until the admin re-enables it.
async fn validate_token(app: &App, token: &str, tun_name: &str) -> Result<(), u16> {
    let conn = app.db.lock().await;
    let row = match pangolin_core::db::auth_tun(&conn, tun_name, token) {
        Ok(r) => r,
        Err(_) => return Err(500),
    };
    let now = chrono::Utc::now();
    match row {
        None => Err(401),
        Some((false, _)) => Err(403),
        Some((true, None)) => Ok(()),
        Some((true, Some(expires_at))) if expires_at > now => Ok(()),
        Some((true, Some(_))) => Err(401), // expired
    }
}

/// Mark a tun as online in the DB.
async fn mark_tun_online(app: &App, tun_name: &str) {
    let conn = app.db.lock().await;
    let _ = pangolin_core::db::set_tun_online(&conn, tun_name, true);
}

/// Mark a tun as offline in the DB.
async fn mark_tun_offline(app: &App, tun_name: &str) {
    let conn = app.db.lock().await;
    let _ = pangolin_core::db::set_tun_online(&conn, tun_name, false);
}

// ---- Main tunnel server ----

/// Start the tunnel WebSocket server on the given address.
/// Runs as an independent background task alongside pingora.
pub async fn start_tunnel_server(
    app: Arc<App>,
    addr: &str,
    shutdown: tokio_util::sync::CancellationToken,
) -> anyhow::Result<()> {
    let addr: SocketAddr = addr
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid tunnel server address {addr:?}: {e}"))?;

    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind tunnel server on {addr}: {e}"))?;

    info!("tunnel server listening on {}", addr);

    loop {
        tokio::select! {
            // Bias toward shutdown so a Ctrl-C during a slow accept
            // doesn't have to wait for the next connection.
            biased;
            _ = shutdown.cancelled() => {
                info!("tunnel: shutdown requested, stopping accept loop");
                return Ok(());
            }
            accept = listener.accept() => {
                match accept {
                    Ok((tcp_stream, client_addr)) => {
                        // Per-conn tasks are aborted wholesale on
                        // runtime shutdown; dropping `tcp_stream`
                        // closes the socket, so no per-conn shutdown
                        // select is needed (and it would risk
                        // cancelling `handle_client` mid-`app.db.lock().await`).
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

// ---------------------------------------------------------------------------
// Service integration
// ---------------------------------------------------------------------------

/// Long-running tunnel WebSocket listener, run by `runtime::Service`.
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

/// Handle a single tunnel client: WebSocket upgrade, auth, then handle requests.
async fn handle_client(
    app: Arc<App>,
    tcp_stream: tokio::net::TcpStream,
    _client_addr: SocketAddr,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Peek to verify this is a GET /tunnel request, and to extract
    // the token/name from the URL for auth. `peek` does NOT advance
    // the read cursor (it's a `MSG_PEEK` under the hood), so the
    // bytes are still available to `accept_async` below — we must
    // NOT call `read` between peek and accept, or accept would see
    // a truncated request and fail the WS handshake.
    //
    // Buffer size: 1024 is comfortably larger than any reasonable
    // GET line (RFC 7230 §3.1.1 doesn't define a hard limit, but
    // production auth tokens can be 64-128+ bytes). `peek` doesn't
    // advance the cursor so reading a larger buffer is cheap.
    let mut peek_buf = [0u8; 1024];
    let peek_n = match tcp_stream.peek(&mut peek_buf).await {
        Ok(0) => return Ok(()),
        Ok(n) => n,
        Err(e) => {
            warn!("peek error: {}", e);
            return Ok(());
        }
    };
    let peek_str = std::str::from_utf8(&peek_buf[..peek_n]).unwrap_or("");
    if !peek_str.starts_with("GET /tunnel") {
        debug!("tunnel: non-GET or wrong path, closing");
        return Ok(());
    }

    // WebSocket handshake (reads the same bytes we peeked)
    let ws_stream = accept_async(tcp_stream).await?;

    // Extract token & name from the URL. The request looks like
    //   GET /tunnel?token=xxx&name=yyy HTTP/1.1\r\n...
    // so the URL is the second whitespace-separated token. We strip
    // the leading `/tunnel?` to get the raw query string.
    let query = peek_str
        .split_whitespace()
        .nth(1)
        .and_then(|url| url.strip_prefix("/tunnel?"))
        .unwrap_or("");

    let mut token = "";
    let mut name = "";
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        match parts.next() {
            Some("token") => token = parts.next().unwrap_or(""),
            Some("name") => name = parts.next().unwrap_or(""),
            _ => {}
        }
    }

    if token.is_empty() || name.is_empty() {
        warn!("tunnel: missing token or name in request");
        return Ok(());
    }

    match validate_token(&app, token, name).await {
        Ok(()) => {}
        Err(status) => {
            // 401: no row matches (name, token) — admin hasn't
            //      provisioned this (name, token) pair yet.
            // 403: row exists but `enabled=0` — admin disabled it.
            // 500: DB error.
            // All three are hard rejects; the tun stays offline
            // until an operator creates / re-enables the row via
            // `POST /tun/new` (the admin UI Tunnels page).
            warn!("tunnel auth failed for {}: status {}", name, status);
            return Ok(());
        }
    }

    let tun_name = name.to_string();
    mark_tun_online(&app, &tun_name).await;
    info!("tun {} connected", tun_name);
    app.add_event(pangolin_core::EventType::TunConnected {
        name: tun_name.clone(),
    });

    handle_tun_ws(app, ws_stream, tun_name).await;
    Ok(())
}

/// Main handler: proxy → tun requests arrive via mpsc channel,
/// write_task reads from mpsc and sends TunnelResponseFrame over WS.
/// ngx also reads frames from WS (for future tun-initiated requests).
async fn handle_tun_ws(
    app: Arc<App>,
    ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    tun_name: String,
) {
    let (ws_sender, mut ws_read) = ws.split();

    // Channel for proxy → tun requests (delivers TunnelMessage with resp_tx)
    let (tx, mut rx) = mpsc::channel::<TunnelMessage>(100);

    // Check for duplicate tun_name — reject if already registered (atomic via entry API)
    {
        let mut sessions = app.tun_sessions.write().await;
        match sessions.entry(tun_name.clone()) {
            std::collections::hash_map::Entry::Occupied(_) => {
                log::warn!("duplicate tun_name '{}' rejected", tun_name);
                let reject =
                    serialize_msgpack(&TunnelFrame::Res(pangolin_core::TunnelResponseFrame {
                        rid: String::new(),
                        status: 409,
                        headers: vec![],
                        body: b"tun name already registered".to_vec(),
                    }));
                if let Err(e) = reject {
                    log::warn!("failed to send duplicate rejection: {}", e);
                }
                return;
            }
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(tx);
            }
        }
    }

    // Pending requests: rid → (resp_tx, insertion_time)
    // Values are periodically cleaned up when they expire (120s > 60s ngx timeout)
    let pending = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::<
        String,
        (
            tokio::sync::oneshot::Sender<pangolin_core::TunnelResponseFrame>,
            std::time::Instant,
        ),
    >::new()));

    // Pending WS relay: rid → resp_tx for WsStart frames (separate from HTTP proxy pending).
    let pending_ws = Arc::new(tokio::sync::Mutex::new(HashMap::<
        String,
        tokio::sync::oneshot::Sender<pangolin_core::TunnelResponseFrame>,
    >::new()));

    let pending_read = pending.clone();
    let write_task = tokio::spawn(async move {
        let mut sender = ws_sender;
        while let Some(msg) = rx.recv().await {
            // msg.body = serialized TunnelRequestFrame (msgpack)
            // Store resp_tx by rid so the read side can route the response
            {
                let mut map = pending_read.lock().await;
                map.insert(msg.rid.clone(), (msg.resp_tx, std::time::Instant::now()));
            }
            let ws_msg = tungstenite::Message::Binary(deflate_encode(&msg.body).into());
            if sender.send(ws_msg).await.is_err() {
                break;
            }
        }
    });

    // Also read frames from the WebSocket:
    // - Req frames from tun: unhandled (architecture is proxy → tun)
    // - Res frames from tun: route to pending request via resp_tx
    // Also periodically clean up expired pending entries (120s > 60s ngx timeout)
    let pending_read = pending.clone();
    let mut cleanup_ticker = tokio::time::interval(std::time::Duration::from_secs(30));
    while let Some(_msg) = ws_read.next().await {
        tokio::select! {
            _ = cleanup_ticker.tick() => {
                // Clean up pending entries older than 120s
                let now = std::time::Instant::now();
                let mut map = pending_read.lock().await;
                map.retain(|_, (_, inserted)| {
                    now.duration_since(*inserted).as_secs() < 120
                });
            }
            msg = futures_util::StreamExt::next(&mut ws_read) => {
                match msg {
                    Some(Ok(tungstenite::Message::Binary(buf))) => {
                        // Try decompress raw DEFLATE, fallback to raw if not compressed
                        let buf = match deflate_decode(&buf) {
                            Ok(d) => d,
                            Err(_) => buf.to_vec(),
                        };
                        match deserialize_msgpack::<TunnelFrame>(&buf) {
                            Ok(TunnelFrame::Req(req_frame)) => {
                                debug!("tun {} → req {} {} (unhandled in proxy→tun architecture)",
                                    tun_name, req_frame.rid, req_frame.path);
                            }
                            Ok(TunnelFrame::Res(resp_frame)) => {
                                debug!("tun {} ← resp {} status={}", tun_name, resp_frame.rid, resp_frame.status);
                                // Route: WS relay (pending_ws) vs HTTP proxy (pending)
                                if let Some(tx) = pending_ws.lock().await.remove(&resp_frame.rid) {
                                    let _ = tx.send(resp_frame);
                                } else {
                                    let mut map = pending.lock().await;
                                    if let Some((tx, _)) = map.remove(&resp_frame.rid) {
                                        let _ = tx.send(resp_frame);
                                    } else {
                                        warn!("no pending request for rid {}", resp_frame.rid);
                                    }
                                }
                            }
                            Ok(TunnelFrame::WsStart { rid, path }) => {
                                debug!("tun {} WsStart rid={} path={}", tun_name, rid, path);
                                // proxy.rs stored resp_tx in pending (even for ws- rids).
                                // Retrieve it and move to pending_ws for WS relay routing.
                                let tx = {
                                    let mut p = pending.lock().await;
                                    p.remove(&rid).map(|(t, _)| t)
                                };
                                if let Some(tx) = tx {
                                    pending_ws.lock().await.insert(rid.clone(), tx);
                                }
                            }
                            Ok(TunnelFrame::WsEnd { rid }) => {
                                debug!("tun {} WsEnd rid={}", tun_name, rid);
                                pending_ws.lock().await.remove(&rid);
                            }
                            Err(e) => {
                                warn!("malformed tunnel frame from {}: {}", tun_name, e);
                            }
                        }
                    }
                    Some(Ok(tungstenite::Message::Text(t))) => {
                        if let Ok(_frame) = serde_json::from_str::<TunnelFrame>(&t) {
                            debug!("tun {} sent JSON frame (decoded but unhandled)", tun_name);
                        } else {
                            warn!("malformed text frame from {}: {}", tun_name, t);
                        }
                    }
                    Some(Ok(tungstenite::Message::Close(_))) => {
                        info!("tun {} sent close", tun_name);
                        break;
                    }
                    Some(Ok(tungstenite::Message::Ping(_))) | Some(Ok(tungstenite::Message::Pong(_))) => {
                        // Ping/pong handled automatically by tungstenite auto-pong
                    }
                    Some(Ok(tungstenite::Message::Frame(_))) => {
                        // WebSocket frame (part of stream) — skip
                    }
                    Some(Err(e)) => {
                        warn!("WS read error from {}: {}", tun_name, e);
                        break;
                    }
                    None => break,
                }
            }
        }
    }

    // Drain pending maps on disconnect (all pending requests get 504)
    // Drain HTTP proxy pending
    {
        let mut map = pending.lock().await;
        for (_, (tx, _)) in map.drain() {
            let _ = tx.send(pangolin_core::TunnelResponseFrame {
                rid: String::new(),
                status: 504,
                headers: vec![],
                body: b"tunnel disconnected".to_vec(),
            });
        }
    }
    // Drain WS relay pending
    {
        let mut map = pending_ws.lock().await;
        for (_, tx) in map.drain() {
            let _ = tx.send(pangolin_core::TunnelResponseFrame {
                rid: String::new(),
                status: 504,
                headers: vec![],
                body: b"tunnel disconnected".to_vec(),
            });
        }
    }

    write_task.abort();

    // Mark offline in DB and unregister from memory
    mark_tun_offline(&app, &tun_name).await;
    app.unregister_tun(&tun_name).await;
    info!("tun {} disconnected", tun_name);
    app.add_event(pangolin_core::EventType::TunDisconnected {
        name: tun_name.clone(),
    });
}
