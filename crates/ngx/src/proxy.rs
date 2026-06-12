//! HTTP proxy via pingora `ProxyHttp` trait.
//!
//! `AppProxy` implements `ProxyHttp` and handles domain-routed proxying.
//! `request_filter` short-circuits for admin API / tunnel routes.
//! Otherwise falls through to `upstream_peer` for direct backends.
//!
//! ## Tunnel path (issue #39)
//!
//! HTTP requests are forwarded to the live tun as one yamux
//! stream per request, carrying raw HTTP/1.1 bytes. The
//! stream is tagged with `0x01` so the tun side knows it's
//! HTTP. WS connections get a separate stream tagged with
//! `0x02`.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use http::header::{HeaderName, HeaderValue};
use log::{debug, error, info, warn};
use pingora::http::{RequestHeader, ResponseHeader};
use pingora::proxy::{ProxyHttp, Session};
use pingora::upstreams::peer::HttpPeer;
use pingora_core::prelude::*;
use tokio::io::AsyncWriteExt;
use tokio::time::{timeout, Duration};

use pangolin_core::tunnel::{
    compute_ws_accept, encode_http_request, pump_ws_relay, read_http_response,
    strip_hop_by_hop_headers, HttpRequest,
};

use crate::App;

const TAG_HTTP: u8 = 0x01;
const TAG_WS: u8 = 0x02;

/// `ProxyHttp` implementation for pangolin.
pub struct AppProxy {
    pub app: Arc<App>,
}

#[async_trait]
impl ProxyHttp for AppProxy {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    async fn request_filter(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<bool> {
        let path = session.req_header().uri.path().to_string();
        let is_ws_upgrade = session.is_upgrade_req();
        log::debug!(
            "PROXY: request_filter path={} is_ws_upgrade={}",
            path,
            is_ws_upgrade
        );

        if is_ws_upgrade && path == self.app.ws_path {
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
                // Tunnel WS: write 101, then run a half-close
                // byte pump between the H1 client stream and a
                // yamux stream over to the tun.
                info!("Tunnel WS relay: {} → tun {}", host, tun_name);
                let mut hdr = ResponseHeader::build(101, None).unwrap();
                hdr.insert_header("Upgrade", "websocket").ok();
                hdr.insert_header("Connection", "Upgrade").ok();
                if let Some(sec_key) = session.get_header("Sec-WebSocket-Key") {
                    if let Ok(key_str) = std::str::from_utf8(sec_key.as_bytes()) {
                        let accept = compute_ws_accept(key_str);
                        hdr.insert_header("Sec-WebSocket-Accept", accept).ok();
                    }
                }
                if let Some(protocols) = session.get_header("Sec-WebSocket-Protocol") {
                    if let Ok(v) = std::str::from_utf8(protocols.as_bytes()) {
                        hdr.insert_header("Sec-WebSocket-Protocol", v).ok();
                    }
                }
                if let Some(version) = session.get_header("Sec-WebSocket-Version") {
                    if let Ok(v) = std::str::from_utf8(version.as_bytes()) {
                        hdr.insert_header("Sec-WebSocket-Version", v).ok();
                    }
                }
                session.write_response_header(Box::new(hdr), false).await?;

                let stream = {
                    let h1 = session.as_http1_mut().expect("not HTTP/1 session");
                    h1.take_stream()
                };

                let tunnel = {
                    let sessions = self.app.tun_sessions.read().await;
                    sessions.get(&tun_name).cloned()
                };
                let Some(tunnel) = tunnel else {
                    warn!("Tun {} not online for WS relay", tun_name);
                    return Ok(true);
                };

                // Open a new yamux stream tagged for WS relay.
                // Send a length-prefixed URL (the request path
                // the client used) so the tun knows where to
                // connect.
                let mut yamux_stream = match tunnel.open_stream().await {
                    Ok(s) => s,
                    Err(e) => {
                        warn!("open yamux stream for WS relay: {}", e);
                        return Ok(true);
                    }
                };
                // Tag + URL.
                let url_field = format!("/{}", path);
                let url_bytes = url_field.as_bytes();
                let len = url_bytes.len() as u16;
                if let Err(e) = async {
                    yamux_stream.write_u8(TAG_WS).await?;
                    yamux_stream.write_all(&len.to_be_bytes()).await?;
                    yamux_stream.write_all(url_bytes).await?;
                    yamux_stream.flush().await?;
                    Ok::<(), std::io::Error>(())
                }
                .await
                {
                    warn!("failed to write WS relay tag/url: {}", e);
                    return Ok(true);
                }

                // Spawn a task that does the half-close byte
                // pump. After this, both `stream` and the
                // yamux stream's other end are moved into the
                // task.
                tokio::spawn(async move {
                    // The yamux StreamHandle implements
                    // AsyncRead+AsyncWrite.
                    let _ = pump_ws_relay(stream, yamux_stream, "ngx", "tun").await;
                });

                return Ok(true);
            }
            // Direct WS: let pingora handle the 101 upgrade.
            return Ok(false);
        }

        // Look up site by Host header.
        let host = host_from_session(session);

        let indexes = self.app.indexes.read().await;
        let site = match pangolin_core::index::lookup_site(&indexes, &host) {
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

        // Tunnel path: open a yamux stream, write the raw
        // HTTP/1.1 request, read the raw response, write it
        // back to the client.
        if !tun_name.is_empty() {
            let tunnel = {
                let sessions = self.app.tun_sessions.read().await;
                sessions.get(&tun_name).cloned()
            };
            if let Some(tunnel) = tunnel {
                debug!("Tunnel routing: {} → tun {}", host, tun_name);
                let method = session.req_header().method.as_str().to_string();
                // path_and_query() (not uri.to_string()) so H1 and H2
                // produce the same on-the-wire path. H2's URI is
                // absolute-form, so to_string() returns
                // "https://host/path" and the backend gets 404. See
                // `real_e2e_tunnel_h2_path_preserved`.
                let req_path = session
                    .req_header()
                    .uri
                    .path_and_query()
                    .map(|pq| pq.as_str().to_string())
                    .unwrap_or_else(|| "/".to_string());
                let full_url = format!(
                    "{}{}",
                    url.trim_end_matches('/'),
                    if req_path.starts_with('/') {
                        req_path.clone()
                    } else {
                        format!("/{}", req_path)
                    }
                );

                // read_request_body() (not read_body_or_idle()): the
                // latter is pingora's internal body-pump primitive and
                // stays pending forever on a body-less keep-alive
                // request (e.g. curl GET with no Content-Length). See
                // `real_e2e_tunnel_get_without_content_length`.
                let mut body_bytes = Vec::new();
                loop {
                    match session.read_request_body().await {
                        Ok(Some(data)) => body_bytes.extend_from_slice(&data),
                        Ok(None) => break,
                        Err(e) => {
                            error!("failed to read request body: {}", e);
                            let _ = session.respond_error(400).await;
                            return Ok(true);
                        }
                    }
                }

                // Build the HTTP/1.1 request bytes.
                let mut headers: Vec<(String, String)> = Vec::new();
                for (k, v) in &session.req_header().headers {
                    headers.push((k.to_string(), v.to_str().unwrap_or("").to_string()));
                }
                // Strip RFC 7230 §6.1 hop-by-op headers before
                // re-serialising to the backend.
                strip_hop_by_hop_headers(&mut headers);

                let req = HttpRequest {
                    method: method.clone(),
                    target: full_url,
                    version: "HTTP/1.1".to_string(),
                    headers,
                    body: body_bytes,
                };
                let req_bytes = encode_http_request(&req);

                // Open a new yamux stream tagged for HTTP.
                let mut yamux_stream = match tunnel.open_stream().await {
                    Ok(s) => s,
                    Err(e) => {
                        warn!("open yamux stream for HTTP: {}", e);
                        let _ = session.respond_error(503).await;
                        return Ok(true);
                    }
                };
                if let Err(e) = async {
                    yamux_stream.write_u8(TAG_HTTP).await?;
                    yamux_stream.write_all(&req_bytes).await?;
                    yamux_stream.flush().await?;
                    // Shutdown the write side so the tun
                    // knows the request is complete.
                    yamux_stream.shutdown().await?;
                    Ok::<(), std::io::Error>(())
                }
                .await
                {
                    warn!("failed to write HTTP request: {}", e);
                    let _ = session.respond_error(503).await;
                    return Ok(true);
                }

                // Read the response with timeout.
                let response = read_http_response(&mut yamux_stream);
                let response = timeout(Duration::from_secs(60), response).await;
                match response {
                    Ok(Ok(resp)) => {
                        // Write the response back to the client.
                        let status = parse_status_from_line(&resp.status_line);
                        let mut hdr = match ResponseHeader::build(status, None) {
                            Ok(h) => h,
                            Err(e) => {
                                error!("failed to build response header: {}", e);
                                let _ = session.respond_error(500).await;
                                return Ok(true);
                            }
                        };
                        for (k, v) in &resp.headers {
                            if let (Ok(name), Ok(value)) = (
                                HeaderName::from_bytes(k.as_bytes()),
                                HeaderValue::from_str(v.as_str()),
                            ) {
                                hdr.insert_header(name, value).ok();
                            }
                        }
                        // Ensure Content-Length is set.
                        if resp
                            .headers
                            .iter()
                            .all(|(k, _)| !k.eq_ignore_ascii_case("content-length"))
                        {
                            hdr.insert_header("Content-Length", resp.body.len().to_string())
                                .ok();
                        }
                        if let Err(e) = session.write_response_header(Box::new(hdr), false).await {
                            error!("failed to write tunnel response header: {}", e);
                            return Ok(true);
                        }
                        if let Err(e) = session
                            .write_response_body(Some(Bytes::from(resp.body)), true)
                            .await
                        {
                            error!("failed to write tunnel response body: {}", e);
                        }
                        Ok(true)
                    }
                    Ok(Err(e)) => {
                        warn!("tunnel response read error: {}", e);
                        let _ = session.respond_error(502).await;
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
        let host = host_from_session(session);

        let indexes = self.app.indexes.read().await;
        let site = match pangolin_core::index::lookup_site(&indexes, &host) {
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

    /// Set Host header per site.host_mode, add X-Forwarded-Host when mode=custom.
    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream: &mut RequestHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        let original_host = host_from_session(session);

        let indexes = self.app.indexes.read().await;
        let site = match pangolin_core::index::lookup_site(&indexes, &original_host) {
            Some(s) => s.clone(),
            None => {
                // Fall back to passthrough
                if !original_host.is_empty() {
                    upstream
                        .insert_header("Host", original_host.as_bytes())
                        .ok();
                }
                return Ok(());
            }
        };
        drop(indexes);

        let backend_host = extract_host_from_backend(&site.backend);

        match site.host_mode {
            pangolin_core::types::HostMode::Backend => {
                // Use backend URL's host (IP or domain) as-is
                if let Some(h) = backend_host {
                    upstream.insert_header("Host", h.as_bytes()).ok();
                }
            }
            pangolin_core::types::HostMode::Passthrough => {
                // Pass through original Host header (default / legacy behavior)
                if !original_host.is_empty() {
                    upstream
                        .insert_header("Host", original_host.as_bytes())
                        .ok();
                }
            }
            pangolin_core::types::HostMode::Custom => {
                // Use custom Host, and add X-Forwarded-Host with the original
                if let Some(ref custom) = site.host_custom {
                    if !custom.is_empty() {
                        upstream.insert_header("Host", custom.as_bytes()).ok();
                    }
                }
                if !original_host.is_empty() {
                    upstream
                        .insert_header("X-Forwarded-Host", original_host.as_bytes())
                        .ok();
                }
            }
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

/// Parse the status code from a status line like
/// "HTTP/1.1 200 OK" → 200.
fn parse_status_from_line(status_line: &str) -> u16 {
    let mut parts = status_line.splitn(3, ' ');
    let _version = parts.next();
    parts
        .next()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(502)
}

/// Extract the effective host from the request, preferring the
/// `Host` header but falling back to the HTTP/2 `:authority`
/// pseudo-header (which pingora exposes via the request URI's
/// authority). Returns an empty string if neither is present.
fn host_from_session(session: &Session) -> String {
    session
        .get_header("Host")
        .and_then(|v| std::str::from_utf8(v.as_bytes()).ok())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            session
                .req_header()
                .uri
                .authority()
                .map(|a| a.as_str().to_string())
        })
        .unwrap_or_default()
}

/// Extract the host part from a backend URL (e.g. "http://1.2.3.4:80" -> "1.2.3.4").
/// Handles the [tun_name:]url format by stripping the optional tun_name prefix.
///
/// Scheme detection: if "://" appears before the first ":", the text before "://"
/// is the URL scheme (http/https), NOT a tun_name. Only when "://" is absent
/// does the code check if the prefix looks like a tun_name.
fn extract_host_from_backend(backend: &str) -> Option<String> {
    // Detect scheme vs tun_name: "://" means it's a URL scheme, not a tun_name prefix
    let url = if let Some(scheme_pos) = backend.find("://") {
        // "://" found — text before it is the scheme (http/https); strip scheme
        let after_scheme = &backend[scheme_pos + 3..];
        Some(after_scheme)
    } else if let Some(pos) = backend.find(':') {
        let (prefix, rest) = backend.split_at(pos);
        // No "://" found — check if prefix looks like a tun_name (all lowercase alphanum)
        if prefix
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        {
            // tun_name: strip "prefix:" then scheme
            let after_tun = rest.strip_prefix(':')?;
            after_tun
                .strip_prefix("http://")
                .or_else(|| after_tun.strip_prefix("https://"))
        } else {
            // Not a tun_name pattern, and no "://" — can't extract host
            None
        }
    } else {
        None
    }?;
    let port_sep = url.find(':').unwrap_or(url.len());
    Some(url[..port_sep].to_string())
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
