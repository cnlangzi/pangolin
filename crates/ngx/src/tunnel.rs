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
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::{accept_async, tungstenite};

use pangolin_core::compress::{deflate_decode, deflate_encode};
use pangolin_core::{deserialize_msgpack, serialize_msgpack, TunnelFrame};

use crate::{App, TunnelMessage};

// ---- Token validation ----

/// Validate token and name against the DB.
/// Returns Ok if valid, Err with status code otherwise.
async fn validate_token(app: &App, token: &str, tun_name: &str) -> Result<(), u16> {
    let conn = app.db.lock().await;
    let tokens = match pangolin_core::db::list_tokens(&conn) {
        Ok(t) => t,
        Err(_) => return Err(500),
    };
    drop(conn);

    let token_rec = tokens.iter().find(|t| {
        t.token == token && t.enabled && t.expires_at.is_none_or(|e| e > chrono::Utc::now())
    });

    if token_rec.is_none() {
        return Err(401);
    }

    // Validate tun name against DB
    let conn = app.db.lock().await;
    let tuns = match pangolin_core::db::list_tuns(&conn) {
        Ok(t) => t,
        Err(_) => return Err(500),
    };
    drop(conn);

    let tun_rec = tuns.iter().find(|t| t.name == tun_name && t.enabled);
    if tun_rec.is_none() {
        return Err(403);
    }

    Ok(())
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
pub async fn start_tunnel_server(app: Arc<App>, addr: &str) {
    let addr: SocketAddr = addr.parse().expect("invalid tunnel server address");

    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            error!("failed to bind tunnel server on {}: {}", addr, e);
            return;
        }
    };

    info!("tunnel server listening on {}", addr);

    loop {
        match listener.accept().await {
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

/// Handle a single tunnel client: WebSocket upgrade, auth, then handle requests.
async fn handle_client(
    app: Arc<App>,
    mut tcp_stream: tokio::net::TcpStream,
    _client_addr: SocketAddr,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Peek to verify this is a GET /tunnel request
    let mut peek_buf = [0u8; 64];
    match tcp_stream.peek(&mut peek_buf).await {
        Ok(0) => return Ok(()),
        Ok(n) => {
            let peek_str = std::str::from_utf8(&peek_buf[..n]).unwrap_or("");
            if !peek_str.starts_with("GET /tunnel") {
                debug!("tunnel: non-GET or wrong path, closing");
                return Ok(());
            }
        }
        Err(e) => {
            warn!("peek error: {}", e);
            return Ok(());
        }
    }

    // Consume peeked bytes
    let mut drain_buf = [0u8; 64];
    let _ = tcp_stream.read(&mut drain_buf).await;

    // WebSocket handshake
    let ws_stream = accept_async(tcp_stream).await?;

    // Extract token & name from the HTTP request path
    let peek_str = std::str::from_utf8(&peek_buf).unwrap_or("");
    let query = peek_str
        .strip_prefix("GET /tunnel")
        .and_then(|s| s.strip_prefix(" HTTP"))
        .map(|s| s.trim())
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

    if let Err(status) = validate_token(&app, token, name).await {
        warn!("tunnel auth failed for {}: status {}", name, status);
        return Ok(());
    }

    // Pre-check for duplicate name (early rejection before WebSocket handshake overhead)
    // Note: The definitive check-and-insert happens atomically in handle_tun_ws
    {
        let sessions = app.tun_sessions.read().await;
        if sessions.contains_key(name) {
            warn!(
                "tunnel: name {} already registered, rejecting duplicate",
                name
            );
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

    // Background task: clean up expired pending entries every 30s
    let pending_clean = pending.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30));
        // Skip first immediate tick
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let now = std::time::Instant::now();
            let mut map = pending_clean.lock().await;
            let before = map.len();
            map.retain(|_, (_, inserted)| now.duration_since(*inserted).as_secs() < 120);
            let removed = before.saturating_sub(map.len());
            if removed > 0 {
                debug!("cleaned {} expired pending requests", removed);
            }
        }
    });

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
    // Skip first immediate tick to avoid cleaning an empty/fresh map
    let _ = cleanup_ticker.tick().await;
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
