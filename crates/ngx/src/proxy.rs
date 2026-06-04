//! HTTP proxy via pingora `ProxyHttp` trait.
//!
//! `AppProxy` implements `ProxyHttp` and handles domain-routed proxying.
//! `request_filter` short-circuits for admin API / tunnel routes.
//! Otherwise falls through to `upstream_peer` for direct backends.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use http::header::{HeaderName, HeaderValue};
use log::{debug, error, info, warn};
use pingora::http::{RequestHeader, ResponseHeader};
use pingora::proxy::{ProxyHttp, Session};
use pingora::upstreams::peer::HttpPeer;
use pingora_core::prelude::*;
use tokio::time::{timeout, Duration};

use crate::{admin_api, App, TunnelMessage};

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

        // WebSocket tunnel path: handle via tunnel
        if path == self.app.ws_path {
            info!("WebSocket tunnel request, upgrading connection");
            // TODO: handle WS upgrade to tunnel handler
            let _ = session.respond_error(426).await;
            return Ok(true);
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
                // read_body returns a borrow; we must copy data before getting req_header.
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

                        // Build pingora response from the frame
                        let mut resp = http::Response::builder().status(response_frame.status);
                        for (k, v) in response_frame.headers.iter() {
                            if let (Ok(name), Ok(value)) = (
                                HeaderName::from_bytes(k.as_bytes()),
                                HeaderValue::from_str(v.as_str()),
                            ) {
                                resp = resp.header(name, value);
                            }
                        }
                        let body = response_frame.body;

                        // Write response header
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
                            .write_response_body(Some(Bytes::from(body)), true)
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
            // nginx对齐: path traversal防护 + 隐藏文件拒绝 + 目录索引(index.html/h) + 条件请求
            // Note: `path` (from uri.path()) has query string already stripped.
            let doc_root = url.trim_start_matches("file://").to_string();
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
                                serve_static_file(session, idx_resolved.to_str().unwrap(), idx_meta, false).await?;
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
                warn!("static file path escapes doc_root: {} (resolved: {})", req_path, resolved_str);
                let _ = session.respond_error(403).await;
                return Ok(true);
            }

            // Hidden file rejection
            let file_name = std::path::Path::new(&resolved).file_name().unwrap_or_default();
            if file_name.to_str().map(|s| s.starts_with('.')).unwrap_or(false) {
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
                        if mtime_val <= SystemTime::from(ims_dt) {
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
    hdr.insert_header("Content-Length", content.len().to_string().as_bytes()).ok();

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
