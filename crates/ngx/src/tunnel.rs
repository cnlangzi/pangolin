//! Tunnel WebSocket endpoint — receives connections from remote tun nodes.
//!
//! ## Protocol (issue #39, yamux-over-fastwebsockets)
//!
//! 1. tun opens a TCP connection to ngx's tunnel listener.
//! 2. tun sends a `GET /tunnel HTTP/1.1` upgrade request with
//!    `Authorization: Bearer <token>` (the token's sha256 is
//!    what's stored in the DB; we hash the inbound bearer
//!    before lookup).
//! 3. ngx validates the bearer via the `tun` table. On
//!    success: send `101 Switching Protocols`. On failure:
//!    send a real HTTP 401/403/500 and close.
//! 4. Once the WS is up, a yamux server session is layered on
//!    top. Each HTTP request from ngx's proxy path opens one
//!    yamux stream carrying raw HTTP/1.1 bytes (no
//!    msgpack, no rid correlation, no DEFLATE on the wire
//!    beyond permessage-deflate). Each WS relay connection
//!    also gets one yamux stream, with a 1-byte tag
//!    (`0x01` = HTTP, `0x02` = WS) to disambiguate.
//! 5. The session is registered in `App::tun_sessions` keyed
//!    by `tun_name`. When a tun disconnects, the entry is
//!    removed and all in-flight HTTP requests get 504.

use std::net::SocketAddr;
use std::sync::Arc;

use fastwebsockets::{Role as WsRole, WebSocket};
use log::{debug, error, info, warn};
use tokio::net::TcpListener;

use pangolin_core::tunnel::{
    TunnelRole, bearer_token, read_ws_upgrade_request, tunnel_over_websocket, write_http_error,
    write_ws_accept_response,
};

use crate::App;

// ---- Auth ----

/// Validate a tun's (name, token) against the `tun` table.
///
/// V3: `auth_tun` hashes the inbound token and compares
/// against `tun.token_hash`. We hash *here* in the call
/// path; the DB lookup is just the equality check.
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
        Some((true, Some(_))) => Err(401),
    }
}

async fn mark_tun_online(app: &App, tun_name: &str) {
    let conn = app.db.lock().await;
    let _ = pangolin_core::db::set_tun_online(&conn, tun_name, true);
}

async fn mark_tun_offline(app: &App, tun_name: &str) {
    let conn = app.db.lock().await;
    let _ = pangolin_core::db::set_tun_online(&conn, tun_name, false);
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

    let listener = TcpListener::bind(addr)
        .await
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

    mark_tun_online(&app, &tun_name).await;
    info!("tun {} connected", tun_name);
    app.add_event(pangolin_core::EventType::TunConnected {
        name: tun_name.clone(),
    });

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

    // Register the session keyed by tun_name. Reject
    // duplicates to avoid two tuns fighting for the same
    // routing slot.
    let session_end = {
        let mut sessions = app.tun_sessions.write().await;
        if sessions.contains_key(&tun_name) {
            warn!("tunnel: duplicate tun_name '{}' rejected", tun_name);
            return Ok(());
        }
        let end = tunnel.session_end.clone();
        sessions.insert(tun_name.clone(), tunnel);
        end
    };

    // Block on the bridge's end-of-session notification.
    // This future resolves when the WebSocket EOFs (the
    // tun dropped the connection). Until then, this
    // per-conn task stays alive and the registry entry
    // is reachable from the proxy hot path.
    session_end.notified().await;

    // Cleanup on disconnect.
    app.unregister_tun(&tun_name).await;
    mark_tun_offline(&app, &tun_name).await;
    info!("tun {} disconnected", tun_name);
    app.add_event(pangolin_core::EventType::TunDisconnected {
        name: tun_name.clone(),
    });
    Ok(())
}
