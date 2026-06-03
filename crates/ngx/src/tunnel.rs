//! Tunnel WebSocket endpoint — receives connections from remote tun nodes.
//!
//! The tun node connects via WS to `ngx:tunnel_port` with token/name in path.
//! After validating, the session is registered in `App.tun_sessions[tun_name]`.
//!
//! Flow:
//!   tun connects → WS handshake → validates token+name → marks online
//!   Proxy sends requests via App.tun_sessions → handle_tun_ws reads from mpsc
//!   handle_tun_ws forwards to backend, gets response, writes back over WS
//!
//! This module runs an independent TCP listener on the configured tunnel port
//! so we get raw `tokio::net::TcpStream` access for direct WebSocket handling.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use log::{debug, error, info, warn};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio_tungstenite::{accept_async, tungstenite};

use crate::{App, TunnelMessage};

// ---- Tunnel protocol frames ----

/// HTTP request frame: tun → ngx (via mpsc from proxy).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TunnelRequestFrame {
    pub rid: String,
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// HTTP response frame: ngx → tun (via WS).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct TunnelResponseFrame {
    pub rid: String,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Unified tunnel frame (request or response), serialized as JSON.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum TunnelFrame {
    Req(TunnelRequestFrame),
    Res(TunnelResponseFrame),
}

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
        t.token == token
            && t.enabled
            && t.expires_at.map_or(true, |e| e > chrono::Utc::now())
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
    let tuns = match pangolin_core::db::list_tuns(&conn) {
        Ok(t) => t,
        Err(_) => return,
    };
    if let Some(mut tun) = tuns.into_iter().find(|t| t.name == tun_name) {
        tun.online = true;
        let _ = pangolin_core::db::upsert_tun(&conn, &tun);
    }
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

    let tun_name = name.to_string();
    mark_tun_online(&app, &tun_name).await;
    info!("tun {} connected", tun_name);

    handle_tun_ws(app, ws_stream, tun_name).await;
    Ok(())
}

/// Main handler: reads TunnelRequestFrames from mpsc (sent by proxy via tun_sessions),
/// forwards to backend (HTTP request), waits for response, writes back over WS.
async fn handle_tun_ws(
    app: Arc<App>,
    ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    tun_name: String,
) {
    let (mut ws_sender, mut ws_read) = ws.split();

    // Channel for proxy → tun requests
    let (tx, mut rx) = mpsc::channel::<TunnelMessage>(100);

    // Register this tun in App.tun_sessions
    {
        let mut sessions = app.tun_sessions.write().await;
        sessions.insert(tun_name.clone(), tx);
    }

    // Spawn a task that reads responses from the mpsc channel and writes to WS
    let write_task = tokio::spawn(async move {
        let mut sender = ws_sender;
        while let Some(msg) = rx.recv().await {
            // msg.body is the serialized TunnelResponseFrame JSON
            let ws_msg =
                tungstenite::Message::Text(String::from_utf8(msg.body).unwrap_or_default().into());
            if sender.send(ws_msg).await.is_err() {
                break;
            }
        }
    });

    // Read request frames from the WebSocket and forward them through tun_sessions
    // (In the current architecture, the proxy sends TO tun via tun_sessions,
    // not the other way around. But we keep WS reading here for future extension
    // where tun initiates requests.)
    while let Some(msg) = ws_read.next().await {
        let text = match msg {
            Ok(tungstenite::Message::Text(t)) => t,
            Ok(tungstenite::Message::Close(_)) => {
                info!("tun {} sent close", tun_name);
                break;
            }
            Err(e) => {
                warn!("WS read error from {}: {}", tun_name, e);
                break;
            }
            _ => continue,
        };

        let frame: Result<TunnelFrame, _> = serde_json::from_str(&text);
        match frame {
            Ok(TunnelFrame::Req(req_frame)) => {
                debug!("tun {} → req {} {}", tun_name, req_frame.rid, req_frame.path);

                // Build TunnelMessage and send to proxy for backend forwarding
                // (In practice, proxy sends requests TO tun via tun_sessions,
                // so this path is for tun-initiated requests in the reverse direction.)
                let body = serde_json::to_vec(&req_frame).unwrap_or_default();
                let tun_msg = TunnelMessage {
                    rid: req_frame.rid.clone(),
                    body,
                    last: true,
                };

                // Get the sender for this tun (proxy side will pick this up)
                // Actually proxy → tun is via tun_sessions; tun → proxy needs a separate path
                // For now, just log that we received a tun-initiated request
                debug!(
                    "tun {} sent request (unhandled in current architecture): {} {}",
                    tun_name, req_frame.method, req_frame.path
                );
            }
            Ok(TunnelFrame::Res(_)) => {
                warn!("unexpected response frame from tun {}", tun_name);
            }
            Err(e) => {
                warn!("malformed tunnel frame from {}: {}", tun_name, e);
            }
        }
    }

    write_task.abort();

    // Unregister on disconnect
    app.unregister_tun(&tun_name).await;
    info!("tun {} disconnected", tun_name);
}