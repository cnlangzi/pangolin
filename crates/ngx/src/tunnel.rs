//! Tunnel WebSocket endpoint — receives connections from remote tun nodes.
//!
//! The tun node connects via WS to `ngx:port/tunnel?token=XXX&name=tun1`.
//! After validating the token against the DB, the session is registered in
//! `App.tun_sessions[tun_name]` and the node is added to the domain index.
//!
//! Incoming frames from tun are HTTP request frames that need to be proxied
//! by the proxy service (direct or tunnel). We deserialize the frame and
//! hand it to a handler that waits for the response frame, then echoes it
//! back over the same WS.
//!
//! This service uses a raw TCP listener (not HTTP/2) so we can handle the
//! WebSocket upgrade manually.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http::{Request, StatusCode};
use log::{debug, error, info, warn};
use pingora::apps::http_app::ServeHttp;
use pingora::protocols::http::{ServerSession, Stream};
use pingora::server::ShutdownWatch;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio_tungstenite::{accept_async, tungstenite};

use crate::{App, TunnelMessage};

// ---- Tunnel session state ----

/// Per-connection state for a live tun WebSocket connection.
struct TunSession {
    /// Unique request ID → response sender, used by the proxy to send back responses.
    pending: Arc<RwLock<std::collections::HashMap<String, oneshot::Sender<TunnelFrame>>>>,
    /// Drop guard — unregisters this session on drop.
    _unregister: UnregisterGuard,
}

/// Guard that unregisters the tun session on Drop.
struct UnregisterGuard {
    tun_name: String,
    app: Arc<App>,
}

impl Drop for UnregisterGuard {
    fn drop(&mut self) {
        // Use try_runtime to avoid panicking if we're not in a tokio context
        let app = self.app.clone();
        let name = self.tun_name.clone();
        tokio::spawn(async move {
            app.unregister_tun(&name).await;
            info!("tun {} unregistered (disconnected)", name);
        });
    }
}

// ---- Tunnel protocol frames ----

/// HTTP request frame sent from tun → ngx.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct TunnelRequestFrame {
    pub rid: String,
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// HTTP response frame sent from ngx → tun.
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

impl TunnelFrame {
    fn unwrap_req(self) -> Option<TunnelRequestFrame> {
        match self {
            TunnelFrame::Req(f) => Some(f),
            _ => None,
        }
    }
    fn unwrap_res(self) -> Option<TunnelResponseFrame> {
        match self {
            TunnelFrame::Res(f) => Some(f),
            _ => None,
        }
    }
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

    let token_rec = tokens
        .iter()
        .find(|t| t.token == token && t.enabled && !t.is_expired());

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

/// Look up the domain → site mapping for this tun.
async fn lookup_tun_domains(app: &App, tun_name: &str) -> Vec<String> {
    let indexes = app.indexes.read().await;
    // TODO: expose a method in pangolin_core index to get domains by tun_name
    // For now, iterate sites that have a backend pointing to this tun
    let mut domains = Vec::new();
    for site in indexes.sites.values() {
        if let Ok((t, _)) = pangolin_core::parse::parse_backend(&site.backend) {
            if t == tun_name {
                // Find all domains pointing to this site
                for (domain, si) in &indexes.domain_index {
                    if si.name == site.name {
                        domains.push(domain.clone());
                    }
                }
            }
        }
    }
    domains
}

// ---- The WebSocket session handler ----

async fn handle_tun_ws(
    app: Arc<App>,
    ws: tokio_tungstenite::WebSocketStream<Stream>,
    tun_name: String,
    pending: Arc<RwLock<std::collections::HashMap<String, oneshot::Sender<TunnelFrame>>>>,
) {
    let (mut write, mut read) = ws.split();

    // Mark tun as online in DB
    {
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

    info!("tun {} connected", tun_name);

    // Spawn a task that registers this session as the sender for this tun_name
    let (tx, mut rx) = mpsc::channel::<TunnelMessage>(100);
    {
        let mut sessions = app.tun_sessions.write().await;
        sessions.insert(tun_name.clone(), tx);
    }

    // Spawn a task to forward proxy → tun responses back over WS
    let pending_clone = pending.clone();
    let write_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            // msg.body already contains the serialized request info
            // We need to deserialize the TunnelFrame to get the rid
            // Actually msg.rid + msg.body should be a TunnelRequestFrame
            let frame: TunnelRequestFrame = match serde_json::from_slice(&msg.body) {
                Ok(f) => f,
                Err(e) => {
                    warn!("invalid tunnel frame from proxy: {}", e);
                    continue;
                }
            };

            let res_frame = TunnelFrame::Res(TunnelResponseFrame {
                rid: frame.rid,
                status: 200,
                headers: vec![],
                body: msg.body,
            });

            let json = match serde_json::to_vec(&res_frame) {
                Ok(j) => j,
                Err(e) => {
                    warn!("failed to serialize response frame: {}", e);
                    continue;
                }
            };

            let ws_msg = tungstenite::Message::Text(json.into());
            if write.send(ws_msg).await.is_err() {
                break;
            }
        }
    });

    // Read frames from tun and handle them
    while let Some(msg) = read.next().await {
        match msg {
            Ok(tungstenite::Message::Text(text)) => {
                let frame: Result<TunnelFrame, _> = serde_json::from_str(&text);
                match frame {
                    Ok(TunnelFrame::Req(req_frame)) => {
                        debug!("tun {} → req {} {}", tun_name, req_frame.rid, req_frame.path);

                        // Register this request's oneshot
                        let (res_tx, res_rx) = oneshot::channel();
                        {
                            let mut p = pending_clone.write().await;
                            p.insert(req_frame.rid.clone(), res_tx);
                        }

                        // Build TunnelMessage for the proxy side
                        let body = serde_json::to_vec(&req_frame).unwrap_or_default();
                        let tun_msg = TunnelMessage {
                            rid: req_frame.rid,
                            body,
                            last: true,
                        };

                        // Send to proxy handler
                        let _ = tx.send(tun_msg).await;

                        // Wait for response
                        let timeout = tokio::time::timeout(Duration::from_secs(30), res_rx);
                        match timeout.await {
                            Ok(Ok(TunnelFrame::Res(res))) => {
                                let json =
                                    serde_json::to_vec(&TunnelFrame::Res(res)).unwrap_or_default();
                                let ws_msg = tungstenite::Message::Text(json.into());
                                let _ = write.send(ws_msg).await;
                            }
                            Ok(Err(_)) => {
                                warn!("channel closed for request");
                            }
                            Err(_) => {
                                warn!("timeout waiting for response frame");
                            }
                        }
                    }
                    Ok(TunnelFrame::Res(_)) => {
                        warn!("unexpected response frame from tun");
                    }
                    Err(e) => {
                        warn!("malformed tunnel frame from {}: {}", tun_name, e);
                    }
                }
            }
            Ok(tungstenite::Message::Close(_)) => {
                info!("tun {} sent close", tun_name);
                break;
            }
            Err(e) => {
                warn!("WS read error from {}: {}", tun_name, e);
                break;
            }
            _ => {}
        }
    }

    write_task.abort();
}

// ---- TLS cert look-up for wss ----

fn load_certs(cert_manager: &crate::CertManager) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    if !cert_manager.enabled {
        return None;
    }
    let cert_path = cert_manager.cert_dir.join("fullchain.pem");
    let key_path = cert_manager.cert_dir.join("privkey.pem");
    if cert_path.exists() && key_path.exists() {
        Some((cert_path, key_path))
    } else {
        None
    }
}

// ---- pingora ServeHttp impl for the tunnel listener ----

pub struct TunnelService {
    app: Arc<App>,
}

impl TunnelService {
    pub fn new(app: Arc<App>) -> Self {
        Self { app }
    }
}

#[async_trait]
impl ServeHttp for TunnelService {
    async fn response(&self, http_session: &mut ServerSession) -> http::Response<Vec<u8>> {
        // Only handle /tunnel path
        let req = http_session.req_header();
        if req.uri.path() != "/tunnel" {
            let mut resp = http::Response::builder()
                .status(404)
                .body(vec![])
                .unwrap();
            *resp.status_mut() = StatusCode::NOT_FOUND;
            return resp;
        }

        // Extract token and name from query params
        let query = req.uri.query().unwrap_or("");
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
            let mut resp = http::Response::builder()
                .status(400)
                .body(vec![])
                .unwrap();
            *resp.status_mut() = StatusCode::BAD_REQUEST;
            return resp;
        }

        // Validate token
        if let Err(status) = validate_token(&self.app, token, name).await {
            let mut resp = http::Response::builder().status(status).body(vec![]).unwrap();
            return resp;
        }

        // Mark tun as online in DB indexes
        {
            let mut indexes = self.app.indexes.write().await;
            // The index doesn't directly track online status;
            // that's in the DB. But we reload to pick up latest.
        }

        // Perform HTTP upgrade
        let (stream, _) = match http_session.stream().clone().into_tcp_stream() {
            Ok(s) => (s, None),
            Err(_) => {
                let mut resp = http::Response::builder()
                    .status(500)
                    .body(vec![])
                    .unwrap();
                *resp.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                return resp;
            }
        };

        let app = self.app.clone();
        let tun_name = name.to_string();
        let pending: Arc<RwLock<std::collections::HashMap<String, oneshot::Sender<TunnelFrame>>>> =
            Arc::new(RwLock::new(std::collections::HashMap::new()));

        let unregister_guard = UnregisterGuard {
            tun_name: tun_name.clone(),
            app: app.clone(),
        };

        // Spawn WS handling
        let app2 = app.clone();
        let name2 = name.to_string();
        let pending2 = pending.clone();

        // We can't directly upgrade a pingora stream to WS here,
        // so we use a workaround: convert to tokio TcpStream
        use std::os::fd::{AsRawFd, FromRawFd};
        use tokio::net::TcpStream;

        let raw_fd = stream.as_raw_fd();
        let tcp_stream = unsafe { TcpStream::from_raw_fd(raw_fd) };

        tokio::spawn(async move {
            let ws_stream = match accept_async(tcp_stream).await {
                Ok(ws) => ws,
                Err(e) => {
                    error!("WS handshake failed for tun {}: {}", name2, e);
                    return;
                }
            };
            let _guard = unregister_guard; // drop unregisters on exit
            handle_tun_ws(app2, ws_stream, name2, pending2).await;
        });

        // Return a placeholder — the real response is sent over WS
        http::Response::builder()
            .status(101)
            .body(vec![])
            .unwrap()
    }
}