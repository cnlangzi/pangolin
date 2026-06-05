//! HTTP proxy via pingora `ProxyHttp` trait.
//!
//! `AppProxy` implements `ProxyHttp` and handles domain-routed proxying.
//! `request_filter` short-circuits for admin API / tunnel routes.
//! Otherwise falls through to `upstream_peer` for direct backends.

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use bytes::Bytes;
use http::header::{HeaderName, HeaderValue};
use log::{debug, error, info, warn};
use pingora::http::{RequestHeader, ResponseHeader};
use pingora::proxy::{ProxyHttp, Session};
use pingora::upstreams::peer::HttpPeer;
use pingora_core::prelude::*;
use sha1::Digest;
use sha1::Sha1;
use tokio::time::{timeout, Duration};
use tokio_tungstenite::tungstenite::protocol::Role;

use crate::{admin_api, App, TunnelMessage};

/// RFC 6454 Sec-WebSocket-Accept computation.
/// Takes the Sec-WebSocket-Key value and returns the correct Accept response.
fn compute_ws_accept(key: &str) -> Option<String> {
    const MAGIC: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    let mut sha = Sha1::new();
    sha.update(format!("{}{}", key.trim(), MAGIC).as_bytes());
    let hash = sha.finalize();
    Some(base64::engine::general_purpose::STANDARD.encode(hash))
}

/// `ProxyHttp` implementation for pangolin.
pub struct AppProxy {
    pub app: Arc<App>,
}

#[async_trait]
impl ProxyHttp for AppProxy {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    /// Request filter — short-circuit for admin API, static files, or tunnel routes.
    ///
    /// Returns `Ok(true)` if we handled the response locally (no upstream proxy).
    /// Returns `Ok(false)` to continue to `upstream_peer`.
    async fn request_filter(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<bool> {
        let path = session.req_header().uri.path().to_string();
        let method = session.req_header().method.as_str().to_string();

        // Admin API: short-circuit, don't proxy
        if path.starts_with("/api/") {
            debug!("Admin API request: {}", path);
            admin_api::handle_api_request(session, &self.app, &path, &method).await?;
            return Ok(true);
        }

        // WebSocket upgrade detection: check Upgrade header
        let is_ws_upgrade = session.is_upgrade_req();

        // WebSocket upgrade detected — determine routing and handle accordingly.
        // Direct WS: return Ok(false) and let pingora's proxy_to_h1_upstream handle 101 upgrade.
        // Tunnel WS: write 101, extract stream, relay through tunnel msgpack channel.
        if is_ws_upgrade && path == self.app.ws_path {
            // Determine if this is a tunnel WS (host has tun_name) or direct WS.
            let host = session
                .get_header("Host")
                .and_then(|v| std::str::from_utf8(v.as_bytes()).ok())
                .unwrap_or("");
            let indexes = self.app.indexes.read().await;
            let tun_name = pangolin_core::index::lookup_site(&indexes, host)
                .and_then(|s| {
                    let (tn, _) = pangolin_core::parse::parse_backend(&s.backend).ok()?;
                    if tn.is_empty() {
                        None
                    } else {
                        Some(tn)
                    }
                })
                .unwrap_or_default();
            drop(indexes);

            if !tun_name.is_empty() {
                // Tunnel WS: relay through existing tun session.
                // 1. Write 101 Switching Protocols to client.
                // 2. Extract stream via into_inner().
                // 3. Send WsStart to tun, relay frames bidirectionally.
                // 4. Send WsEnd when done.
                info!("Tunnel WS relay: {} → tun {}", host, tun_name);
                let mut hdr = ResponseHeader::build(101, None).unwrap();
                hdr.insert_header("Upgrade", "websocket").unwrap();
                hdr.insert_header("Connection", "Upgrade").unwrap();
                // Compute Sec-WebSocket-Accept per RFC 6454
                if let Some(sec_key) = session.get_header("Sec-WebSocket-Key") {
                    if let Ok(key_str) = std::str::from_utf8(sec_key.as_bytes()) {
                        if let Some(accept) = compute_ws_accept(key_str) {
                            hdr.insert_header("Sec-WebSocket-Accept", accept.as_bytes())
                                .ok();
                        }
                    }
                }
                if let Some(protocols) = session.get_header("Sec-WebSocket-Protocol") {
                    if let Ok(v) = std::str::from_utf8(protocols.as_bytes()) {
                        hdr.insert_header("Sec-WebSocket-Protocol", v.as_bytes())
                            .ok();
                    }
                }
                if let Some(version) = session.get_header("Sec-WebSocket-Version") {
                    if let Ok(v) = std::str::from_utf8(version.as_bytes()) {
                        hdr.insert_header("Sec-WebSocket-Version", v.as_bytes())
                            .ok();
                    }
                }
                session.write_response_header(Box::new(hdr), false).await?;

                // Extract stream using take_stream() from our patched pingora fork.
                // This takes &mut self (not ownership), making it safe for request_filter use.
                // After this, the session is inert — we return Ok(true) immediately,
                // so pingora's finish() sees an empty stream and handles it safely.
                let stream = {
                    let h1 = session.as_http1_mut().expect("not HTTP/1 session");
                    h1.take_stream()
                };

                // Get or check tunnel sender.
                let sender = {
                    let sessions = self.app.tun_sessions.read().await;
                    sessions.get(&tun_name).cloned()
                };
                let Some(sender) = sender else {
                    warn!("Tun {} not online for WS relay", tun_name);
                    return Ok(true);
                };

                // Spawn bidirectional relay task.
                // We cannot capture session across await (Session is !Send), but we extracted
                // the stream before the await, so this is safe: stream is owned by 'static task.
                let rid = format!(
                    "ws-{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                );
                tokio::spawn(async move {
                    use futures_util::{SinkExt, StreamExt};
                    use tokio_tungstenite::{connect_async, WebSocketStream};

                    use tokio_tungstenite::tungstenite::Message;

                    // Establish connection to backend through tunnel.
                    // First, send WsStart to tunnel via existing msgpack channel.
                    let (resp_tx, resp_rx) =
                        tokio::sync::oneshot::channel::<pangolin_core::TunnelResponseFrame>();
                    let ws_start_frame = pangolin_core::TunnelFrame::WsStart {
                        rid: rid.clone(),
                        path: path.clone(),
                    };
                    let start_body =
                        pangolin_core::serialize_msgpack(&ws_start_frame).unwrap_or_default();
                    let msg = TunnelMessage {
                        rid: rid.clone(),
                        body: start_body,
                        resp_tx,
                    };
                    if sender.send(msg).await.is_err() {
                        return;
                    }
                    // Wait for tunnel to establish backend connection.
                    let backend_addr = match resp_rx.await {
                        Ok(resp) if resp.status == 101 => {
                            // resp.body contains "host:port" of backend
                            String::from_utf8_lossy(&resp.body).to_string()
                        }
                        Ok(resp) => {
                            error!("tunnel WS start failed: status {}", resp.status);
                            return;
                        }
                        Err(_) => {
                            error!("tunnel WS start: no response");
                            return;
                        }
                    };

                    // Connect to backend WebSocket (URL now includes ws:// or wss:// scheme).
                    let (ws_outbound, _) = match connect_async(&backend_addr).await {
                        Ok(c) => c,
                        Err(e) => {
                            error!("WS connect to backend {} failed: {}", backend_addr, e);
                            return;
                        }
                    };

                    // Wrap our stream in a tokio_tungstenite WebSocketStream.
                    let (client_ws_sender, mut client_ws_read) = {
                        let ws_stream =
                            WebSocketStream::from_raw_socket(stream, Role::Server, None).await;
                        ws_stream.split()
                    };

                    // Bidirectional relay: client ↔ backend.
                    let (mut out_sender, mut out_read) = ws_outbound.split();
                    let mut client_sender = client_ws_sender;

                    loop {
                        tokio::select! {
                            // Client → Backend
                            msg = client_ws_read.next() => {
                                match msg {
                                    Some(Ok(Message::Binary(data))) => {
                                        // Forward raw WS frame to backend (no msgpack wrapping).
                                        if out_sender.send(Message::Binary(data)).await.is_err() {
                                            break;
                                        }
                                    }
                                    Some(Ok(Message::Text(data))) => {
                                        if out_sender.send(Message::Text(data)).await.is_err() {
                                            break;
                                        }
                                    }
                                    Some(Ok(Message::Close(_))) | None => {
                                        let _ = out_sender.send(Message::Close(None)).await;
                                        break;
                                    }
                                    Some(Ok(Message::Ping(d))) => {
                                        if out_sender.send(Message::Pong(d)).await.is_err() {
                                            break;
                                        }
                                    }
                                    Some(Ok(Message::Pong(_))) | Some(Ok(Message::Frame(_))) => {}
                                    Some(Err(e)) => {
                                        error!("client WS read error: {}", e);
                                        break;
                                    }
                                }
                            }
                            // Backend → Client
                            msg = out_read.next() => {
                                match msg {
                                    Some(Ok(Message::Binary(data))) => {
                                        // Forward raw WS frame to client (no msgpack parsing).
                                        let _ = client_sender.send(Message::Binary(data)).await;
                                    }
                                    Some(Ok(Message::Text(t))) => {
                                        let _ = client_sender.send(Message::Text(t)).await;
                                    }
                                    Some(Ok(Message::Close(_))) | None => {
                                        let _ = client_sender.send(Message::Close(None)).await;
                                        break;
                                    }
                                    Some(Ok(Message::Ping(d))) => {
                                        let _ = client_sender.send(Message::Pong(d)).await;
                                    }
                                    Some(Ok(Message::Pong(_))) | Some(Ok(Message::Frame(_))) => {}
                                    Some(Err(e)) => {
                                        error!("backend WS read error: {}", e);
                                        break;
                                    }
                                }
                            }
                        }
                    }

                    // Send WsEnd to tunnel.
                    let ws_end = pangolin_core::TunnelFrame::WsEnd { rid };
                    let end_body = pangolin_core::serialize_msgpack(&ws_end).unwrap_or_default();
                    let _ = sender
                        .send(TunnelMessage {
                            rid: "ws-end".into(),
                            body: end_body,
                            resp_tx: tokio::sync::oneshot::channel().0,
                        })
                        .await;
                });

                return Ok(true);
            }
            // Direct WS: let pingora handle the 101 upgrade.
            return Ok(false);
        }

        // Look up site by Host header
        let host = session
            .get_header("Host")
            .and_then(|v| std::str::from_utf8(v.as_bytes()).ok())
            .unwrap_or("");

        let indexes = self.app.indexes.read().await;
        let site = match pangolin_core::index::lookup_site(&indexes, host) {
            Some(s) => s.clone(),
            None => {
                debug!("No site found for host: {}", host);
                let _ = session.respond_error(404).await;
                return Ok(true);
            }
        };
        drop(indexes);

        // Parse the backend to determine routing type
        let backend_str = site.backend.clone();
        let (tun_name, url) = match pangolin_core::parse::parse_backend(&backend_str) {
            Ok((t, u)) => (t, u),
            Err(e) => {
                error!("Invalid backend for site {}: {}", site.name, e);
                let _ = session.respond_error(502).await;
                return Ok(true);
            }
        };

        // Tunnel path: forward request to the live tun session
        if !tun_name.is_empty() {
            let sender = {
                let sessions = self.app.tun_sessions.read().await;
                sessions.get(&tun_name).cloned()
            };
            if let Some(sender) = sender {
                debug!("Tunnel routing: {} → tun {}", host, tun_name);
                let rid = format!(
                    "req-{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                );

                // Build full request frame with all headers and body.
                let body_bytes = {
                    match session.read_body_or_idle(false).await {
                        Ok(Some(data)) => data.to_vec(),
                        Ok(None) => Vec::new(),
                        Err(e) => {
                            error!("failed to read request body: {}", e);
                            let _ = session.respond_error(400).await;
                            return Ok(true);
                        }
                    }
                };

                let req_header = session.req_header();
                let mut headers: Vec<(String, String)> = Vec::new();
                for (k, v) in &req_header.headers {
                    headers.push((k.to_string(), v.to_str().unwrap_or("").to_string()));
                }

                let req_frame = pangolin_core::TunnelRequestFrame {
                    rid: rid.clone(),
                    method: method.clone(),
                    path: req_header.uri.to_string(),
                    headers,
                    body: body_bytes,
                };

                let buf = match pangolin_core::serialize_msgpack(&req_frame) {
                    Ok(b) => b,
                    Err(e) => {
                        error!("failed to serialize request: {}", e);
                        let _ = session.respond_error(500).await;
                        return Ok(true);
                    }
                };

                // Create oneshot channel to receive response
                let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();

                let msg = TunnelMessage {
                    rid: rid.clone(),
                    body: buf,
                    resp_tx,
                };

                // Send request and wait for response (with timeout)
                let response = async {
                    sender.send(msg).await.map_err(|_| "tun disconnected")?;
                    resp_rx.await.map_err(|_| "response channel closed")
                };

                match timeout(Duration::from_secs(60), response).await {
                    Ok(Ok(response_frame)) => {
                        // Got response from tun — write it to client
                        debug!(
                            "tunnel response {} bytes for rid {}",
                            response_frame.body.len(),
                            rid
                        );

                        let status = response_frame.status;
                        let mut hdr = match ResponseHeader::build(status, None) {
                            Ok(h) => h,
                            Err(e) => {
                                error!("failed to build response header: {}", e);
                                let _ = session.respond_error(500).await;
                                return Ok(true);
                            }
                        };
                        for (k, v) in response_frame.headers.iter() {
                            if let (Ok(name), Ok(value)) = (
                                HeaderName::from_bytes(k.as_bytes()),
                                HeaderValue::from_str(v.as_str()),
                            ) {
                                hdr.insert_header(name, value).ok();
                            }
                        }
                        if let Err(e) = session.write_response_header(Box::new(hdr), true).await {
                            error!("failed to write tunnel response header: {}", e);
                            let _ = session.respond_error(500).await;
                            return Ok(true);
                        }
                        if let Err(e) = session
                            .write_response_body(Some(Bytes::from(response_frame.body)), true)
                            .await
                        {
                            error!("failed to write tunnel response body: {}", e);
                        }
                        Ok(true)
                    }
                    Ok(Err(e)) => {
                        warn!("tunnel send error: {}", e);
                        let _ = session.respond_error(503).await;
                        Ok(true)
                    }
                    Err(_) => {
                        warn!("tunnel timeout for tun {}", tun_name);
                        let _ = session.respond_error(504).await;
                        Ok(true)
                    }
                }
            } else {
                warn!("Tun {} not online", tun_name);
                let _ = session.respond_error(503).await;
                Ok(true)
            }
        } else if url.starts_with("file:///") {
            // Static file serving (file:///doc_root/...)
            let doc_root = pangolin_core::parse::file_url_to_path(&url)
                .unwrap_or(&url)
                .to_string();
            let req_path = path.as_str();

            // Build the file system path
            let file_path_str = if req_path == "/" {
                doc_root.clone()
            } else {
                format!("{}{}", doc_root, req_path)
            };

            // Path traversal check: reject any ".." segment
            if req_path.contains("..") {
                warn!("static file path traversal attempt: {}", req_path);
                let _ = session.respond_error(400).await;
                return Ok(true);
            }

            // Resolve real path and verify it stays within doc_root
            let resolved = match std::fs::canonicalize(&file_path_str) {
                Ok(p) => p,
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        // Try index.html / index.htm for directory request
                        if req_path.ends_with("/") {
                            let idx_path = format!("{}index.html", file_path_str);
                            if std::path::Path::new(&idx_path).exists() {
                                let idx_resolved = std::fs::canonicalize(&idx_path).unwrap();
                                let idx_meta = std::fs::metadata(&idx_resolved).unwrap();
                                serve_static_file(
                                    session,
                                    idx_resolved.to_str().unwrap(),
                                    idx_meta,
                                    false,
                                )
                                .await?;
                                return Ok(true);
                            }
                            let idx_htm_path = format!("{}index.htm", file_path_str);
                            if std::path::Path::new(&idx_htm_path).exists() {
                                let idx_meta = std::fs::metadata(&idx_htm_path).unwrap();
                                serve_static_file(session, &idx_htm_path, idx_meta, false).await?;
                                return Ok(true);
                            }
                        }
                        let _ = session.respond_error(404).await;
                        return Ok(true);
                    }
                    error!("static file canonicalize error {}: {}", file_path_str, e);
                    let _ = session.respond_error(500).await;
                    return Ok(true);
                }
            };

            let resolved_str = resolved.to_str().unwrap();
            let doc_root_resolved = std::fs::canonicalize(&doc_root).unwrap();

            // Verify resolved path is within doc_root
            if !resolved_str.starts_with(doc_root_resolved.to_str().unwrap()) {
                warn!(
                    "static file path escapes doc_root: {} (resolved: {})",
                    req_path, resolved_str
                );
                let _ = session.respond_error(403).await;
                return Ok(true);
            }

            // Hidden file rejection
            let file_name = std::path::Path::new(&resolved)
                .file_name()
                .unwrap_or_default();
            if file_name
                .to_str()
                .map(|s| s.starts_with('.'))
                .unwrap_or(false)
            {
                warn!("static file hidden file rejection: {}", resolved_str);
                let _ = session.respond_error(403).await;
                return Ok(true);
            }

            let meta = match std::fs::metadata(&resolved) {
                Ok(m) => m,
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        let _ = session.respond_error(404).await;
                    } else {
                        let _ = session.respond_error(500).await;
                    }
                    return Ok(true);
                }
            };

            // Directory request: try index.html/h first
            if meta.is_dir() {
                let idx_html = format!("{}/index.html", resolved_str);
                let idx_htm = format!("{}/index.htm", resolved_str);
                if std::path::Path::new(&idx_html).exists() {
                    let idx_meta = std::fs::metadata(&idx_html).unwrap();
                    serve_static_file(session, &idx_html, idx_meta, true).await?;
                    return Ok(true);
                }
                if std::path::Path::new(&idx_htm).exists() {
                    let idx_meta = std::fs::metadata(&idx_htm).unwrap();
                    serve_static_file(session, &idx_htm, idx_meta, true).await?;
                    return Ok(true);
                }
                // No index found — 404 (no directory listing)
                let _ = session.respond_error(404).await;
                return Ok(true);
            }

            serve_static_file(session, resolved_str, meta, true).await?;
            return Ok(true);
        } else {
            // Direct path: continue to upstream_peer (return Ok(false))
            debug!("Direct proxy: {} → {}", host, url);
            Ok(false)
        }
    }

    /// Select the upstream peer based on the site backend URL.
    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let host = session
            .get_header("Host")
            .and_then(|v| std::str::from_utf8(v.as_bytes()).ok())
            .unwrap_or("");

        let indexes = self.app.indexes.read().await;
        let site = match pangolin_core::index::lookup_site(&indexes, host) {
            Some(s) => s.clone(),
            None => {
                error!("No site for host: {}", host);
                return Err(Error::new_str("site not found"));
            }
        };
        drop(indexes);

        let url = match pangolin_core::parse::parse_backend(&site.backend) {
            Ok((_, u)) => u,
            Err(e) => {
                return Err(Error::explain(
                    ErrorType::ReadError,
                    format!("bad backend: {}", e),
                ));
            }
        };

        // Determine TLS and address based on scheme
        let (address, tls, sni) = if url.starts_with("https://") {
            let addr = url.trim_start_matches("https://");
            let port_sep = addr.find(':').unwrap_or(addr.len());
            let host_part = &addr[..port_sep];
            let port: u16 = addr[port_sep + 1..]
                .trim_start_matches(':')
                .parse()
                .unwrap_or(443);
            (
                format!("{}:{}", host_part, port),
                true,
                host_part.to_string(),
            )
        } else if url.starts_with("http://") {
            let addr = url.trim_start_matches("http://");
            let port_sep = addr.find(':').unwrap_or(addr.len());
            let host_part = &addr[..port_sep];
            let port: u16 = addr[port_sep + 1..]
                .trim_start_matches(':')
                .parse()
                .unwrap_or(80);
            (format!("{}:{}", host_part, port), false, String::new())
        } else {
            return Err(Error::new_str("unsupported backend scheme"));
        };

        let peer = HttpPeer::new(address, tls, sni);
        Ok(Box::new(peer))
    }

    /// Preserve original Host header for upstream (important for vhosting).
    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream: &mut RequestHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        if let Some(host) = session.get_header("Host") {
            upstream.insert_header("Host", host).ok();
        }
        Ok(())
    }

    /// Response filter — could add headers or log here.
    async fn response_filter(
        &self,
        _session: &mut Session,
        _upstream_response: &mut ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        Ok(())
    }
}

/// Serve a static file with MIME type, ETag, and Last-Modified support.
/// Handles If-None-Match (ETag) and If-Modified-Since for conditional responses.
async fn serve_static_file(
    session: &mut Session,
    file_path: &str,
    meta: std::fs::Metadata,
    apply_conditional: bool,
) -> Result<()> {
    use std::time::SystemTime;

    let mime = mime_guess::from_path(file_path)
        .first_or_octet_stream()
        .to_string();

    // Build ETag from mtime + size (like nginx)
    let mtime = meta.modified().ok();
    let etag = mtime.map(|t| {
        let dur = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
        format!("\"{}x{}\"", meta.len(), dur.as_secs())
    });

    let req_header = session.req_header();

    // Conditional request: check If-None-Match
    if apply_conditional {
        if let Some(etag_val) = &etag {
            if let Some(inm) = req_header.headers.get("If-None-Match") {
                if let Ok(inm_str) = std::str::from_utf8(inm.as_bytes()) {
                    if inm_str == etag_val.as_str() || inm_str == "*" {
                        // ETag match → 304 Not Modified
                        let mut hdr = match ResponseHeader::build(304, None) {
                            Ok(h) => h,
                            Err(e) => {
                                error!("failed to build 304 response header: {}", e);
                                return Ok(());
                            }
                        };
                        hdr.insert_header("ETag", etag_val.as_bytes()).ok();
                        hdr.insert_header("Content-Type", mime.as_bytes()).ok();
                        let _ = session.write_response_header(Box::new(hdr), true).await;
                        return Ok(());
                    }
                }
            }
        }

        // If-Modified-Since (used when ETag is not available)
        if let Some(mtime_val) = mtime {
            if let Some(ims) = req_header.headers.get("If-Modified-Since") {
                if let Ok(ims_str) = std::str::from_utf8(ims.as_bytes()) {
                    if let Ok(ims_dt) = httpdate::parse_http_date(ims_str) {
                        if mtime_val <= ims_dt {
                            let mut hdr = match ResponseHeader::build(304, None) {
                                Ok(h) => h,
                                Err(e) => {
                                    error!("failed to build 304 response header: {}", e);
                                    return Ok(());
                                }
                            };
                            let dt = httpdate::fmt_http_date(mtime_val);
                            hdr.insert_header("Last-Modified", dt.as_bytes()).ok();
                            let _ = session.write_response_header(Box::new(hdr), true).await;
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    // Read file content
    let content = match tokio::fs::read(file_path).await {
        Ok(c) => c,
        Err(e) => {
            error!("static file read error {}: {}", file_path, e);
            let _ = session.respond_error(500).await;
            return Ok(());
        }
    };

    let mut hdr = match ResponseHeader::build(200, None) {
        Ok(h) => h,
        Err(e) => {
            error!("failed to build response header: {}", e);
            let _ = session.respond_error(500).await;
            return Ok(());
        }
    };

    hdr.insert_header("Content-Type", mime.as_bytes()).ok();
    hdr.insert_header("Content-Length", content.len().to_string().as_bytes())
        .ok();

    if let Some(etag_val) = &etag {
        hdr.insert_header("ETag", etag_val.as_bytes()).ok();
    }

    if let Some(mtime_val) = mtime {
        let dt = httpdate::fmt_http_date(mtime_val);
        hdr.insert_header("Last-Modified", dt.as_bytes()).ok();
    }

    // Cache-Control: no-cache to match nginx default for static files
    hdr.insert_header("Cache-Control", "no-cache").ok();

    if let Err(e) = session.write_response_header(Box::new(hdr), true).await {
        error!("failed to write response header: {}", e);
        let _ = session.respond_error(500).await;
        return Ok(());
    }

    if let Err(e) = session
        .write_response_body(Some(Bytes::from(content)), true)
        .await
    {
        error!("failed to write response body: {}", e);
    }

    Ok(())
}
