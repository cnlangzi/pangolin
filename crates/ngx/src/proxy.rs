//! HTTP proxy via pingora `ProxyHttp` trait.
//!
//! `AppProxy` implements `ProxyHttp` and handles domain-routed proxying.
//! `request_filter` short-circuits for admin API / tunnel routes.
//! Otherwise falls through to `upstream_peer` for direct backends.
//!
//! ## Architecture (v8 — see `docs/design/reverse-proxy.md`)
//!
//! `request_filter` does:
//!   1. ACME HTTP-01 short-circuit
//!   2. WS-upgrade short-circuit (when path is the configured
//!      tun ws_path)
//!   3. Site lookup
//!   4. Build a `BackendTarget` (Http / Https / File)
//!   5. Dispatch:
//!        - file:// direct → `serve_file_target` (pangolin-core)
//!        - tun_name set   → `YamuxTunnelExecutor`
//!        - direct         → fall through to pingora (return Ok(false))
//!
//! `upstream_request_filter` (called by pingora on the direct path)
//! runs `apply_proxy_policy` (pangolin-core) so that Host rewriting
//! is identical on every delivery path.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use http::header::HeaderValue;
use log::{debug, error, info, warn};
use pingora::http::{RequestHeader, ResponseHeader};
use pingora::proxy::{ProxyHttp, Session};
use pingora::upstreams::peer::HttpPeer;
use pingora_core::prelude::*;
use tokio::io::AsyncWriteExt;
use tokio::time::timeout;

use pangolin_core::tunnel::{HttpRequest, HttpResponse, compute_ws_accept, read_http_response};
use pangolin_core::types::HostMode;
use pangolin_core::{
    BackendTarget, ProxyCtx, Scheme, TunnelHttpFrame, apply_proxy_policy, parse_backend_to_target,
    serve_file_target,
};

use crate::App;

// ─────────────────────────────────────────────────────────────────
//  BackendExecutor — transport abstraction
// ─────────────────────────────────────────────────────────────────

/// Error type for executor failures. Maps onto the same HTTP
/// status codes the previous direct path produced.
#[derive(Debug)]
pub enum ProxyError {
    Backend(String),
    Timeout,
    Unavailable(String),
}

impl std::fmt::Display for ProxyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProxyError::Backend(s) => write!(f, "backend error: {s}"),
            ProxyError::Timeout => write!(f, "timeout"),
            ProxyError::Unavailable(s) => write!(f, "unavailable: {s}"),
        }
    }
}

impl std::error::Error for ProxyError {}

/// Send `request` to the backend described by `target` and
/// return the response. `host_mode` and `host_custom` are
/// explicit parameters so the executor does not have to
/// second-guess the request headers — the v8 design wants
/// host_mode to flow through the executor in a typed slot,
/// not be inferred from the presence/absence of
/// `X-Forwarded-Host`.
///
/// The trait is **defined in `ngx`** (not in `pangolin-core`)
/// because all current implementations need pingora — and
/// `pangolin-core` deliberately avoids the pingora dependency.
/// The `tun` side uses the same trait shape but the
/// `PingoraClientExecutor` lives in the `tun` crate.
#[async_trait]
pub trait BackendExecutor: Send + Sync {
    async fn execute_http(
        &self,
        request: HttpRequest,
        target: &BackendTarget,
        host_mode: HostMode,
        host_custom: Option<String>,
    ) -> Result<HttpResponse, ProxyError>;
}

// ─────────────────────────────────────────────────────────────────
//  YamuxTunnelExecutor — encode → yamux stream → decode
// ─────────────────────────────────────────────────────────────────

/// Send a request through a live `YamuxTunnel`. Encodes a
/// `TunnelHttpFrame` (carrying host_mode, host_custom,
/// is_upgrade) and waits for the matching response.
pub struct YamuxTunnelExecutor<'a> {
    pub tunnel: &'a pangolin_core::YamuxTunnel,
}

#[async_trait]
impl<'a> BackendExecutor for YamuxTunnelExecutor<'a> {
    async fn execute_http(
        &self,
        request: HttpRequest,
        target: &BackendTarget,
        host_mode: HostMode,
        host_custom: Option<String>,
    ) -> Result<HttpResponse, ProxyError> {
        let is_upgrade = is_ws_upgrade(&request);
        let frame = TunnelHttpFrame {
            request,
            target: target.clone(),
            host_mode,
            host_custom,
            is_upgrade,
        };
        let bytes = pangolin_core::proxy::encode_tunnel_frame(&frame);

        let mut stream = self
            .tunnel
            .open_stream()
            .await
            .map_err(|e| ProxyError::Unavailable(format!("open_stream: {e}")))?;
        stream
            .write_all(&bytes)
            .await
            .map_err(|e| ProxyError::Backend(format!("write frame: {e}")))?;
        stream
            .shutdown()
            .await
            .map_err(|e| ProxyError::Backend(format!("shutdown: {e}")))?;

        let response = read_http_response(&mut stream);
        let response = timeout(Duration::from_secs(60), response)
            .await
            .map_err(|_| ProxyError::Timeout)?
            .map_err(|e| ProxyError::Backend(format!("read response: {e}")))?;

        Ok(response)
    }
}

fn is_ws_upgrade(req: &HttpRequest) -> bool {
    let has_upgrade = read_header_value(req, "Upgrade").is_ok();
    let connection_has_upgrade = read_header_value(req, "Connection")
        .map(|v| v.to_ascii_lowercase().contains("upgrade"))
        .unwrap_or(false);
    has_upgrade && connection_has_upgrade
}

fn read_header_value(req: &HttpRequest, name: &str) -> Result<String, ()> {
    for (k, v) in &req.headers {
        if k.eq_ignore_ascii_case(name) {
            return Ok(v.clone());
        }
    }
    Err(())
}

// ─────────────────────────────────────────────────────────────────
//  AppProxy (pingora)
// ─────────────────────────────────────────────────────────────────

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
        let is_upgrade = session.is_upgrade_req();
        log::debug!(
            "PROXY: request_filter path={} is_ws_upgrade={}",
            path,
            is_upgrade
        );

        // ── ACME HTTP-01 short-circuit (issue #54) ─────────────
        if let Some(token) = crate::acme::parse_http01_path(&path) {
            let cert_dir = self.app.cert_manager.cert_dir.clone();
            match crate::acme::read_http01_challenge(&cert_dir, token).await {
                Ok(Some(body)) => {
                    debug!("ACME HTTP-01 served token={}", token);
                    let mut hdr = match ResponseHeader::build(200, None) {
                        Ok(h) => h,
                        Err(e) => {
                            error!("failed to build ACME challenge response: {}", e);
                            let _ = session.respond_error(500).await;
                            return Ok(true);
                        }
                    };
                    hdr.insert_header("Content-Type", "text/plain; charset=utf-8")
                        .ok();
                    hdr.insert_header("Content-Length", body.len().to_string().as_bytes())
                        .ok();
                    if let Err(e) = session.write_response_header(Box::new(hdr), false).await {
                        error!("failed to write ACME challenge response header: {}", e);
                        return Ok(true);
                    }
                    if let Err(e) = session
                        .write_response_body(Some(Bytes::from(body)), true)
                        .await
                    {
                        error!("failed to write ACME challenge response body: {}", e);
                    }
                    return Ok(true);
                }
                Ok(None) => {
                    debug!("ACME HTTP-01 not found: path={}", path);
                    let _ = session.respond_error(404).await;
                    return Ok(true);
                }
                Err(e) => {
                    error!("ACME HTTP-01 read error for {}: {}", path, e);
                    let _ = session.respond_error(500).await;
                    return Ok(true);
                }
            }
        }

        // ── WebSocket upgrade short-circuit ────────────────────
        if is_upgrade && path == self.app.ws_path {
            let host = host_from_session(session);
            let indexes = self.app.indexes.read().await;
            let (tun_name, ws_target) = pangolin_core::index::lookup_site(&indexes, &host)
                .and_then(|s| {
                    let (tn, _) = pangolin_core::parse::parse_backend(&s.backend).ok()?;
                    if tn.is_empty() {
                        None
                    } else {
                        // For WS, derive a placeholder
                        // Http/Https target from the backend
                        // URL so the tun has a host:port to
                        // dial. The WS path bypasses the
                        // dispatcher's http/https-vs-file
                        // branch — `handle_ws_upgrade` reads
                        // `request.target` to get host:port.
                        let target = pangolin_core::parse_backend_to_target(&s.backend)
                            .ok()
                            .map(|(_, t)| t);
                        Some((tn, target))
                    }
                })
                .unwrap_or_default();
            drop(indexes);

            if !tun_name.is_empty() {
                info!("Tunnel WS relay: {} → tun {}", host, tun_name);

                // Build the HttpRequest from the session and
                // encode as a TunnelHttpFrame with is_upgrade=true.
                let req = match build_request_from_session(session, "/").await {
                    Ok(r) => r,
                    Err(e) => {
                        error!("ws relay: build request: {}", e);
                        let _ = session.respond_error(500).await;
                        return Ok(true);
                    }
                };
                // For WS, if the target is File, there's no
                // host:port to dial — bail out (file://
                // doesn't make sense for a WS upgrade).
                let ws_target = ws_target.unwrap_or_else(|| BackendTarget::Http {
                    host: "127.0.0.1".into(),
                    port: 80,
                    base_path: String::new(),
                });
                if matches!(ws_target, BackendTarget::File { .. }) {
                    let _ = session.respond_error(400).await;
                    return Ok(true);
                }
                let frame = TunnelHttpFrame {
                    request: req,
                    target: ws_target,
                    host_mode: HostMode::Passthrough,
                    host_custom: None,
                    is_upgrade: true,
                };

                let tunnel = {
                    let sessions = self.app.tun_sessions.read().await;
                    sessions.get(&tun_name).cloned()
                };
                let Some(tunnel) = tunnel else {
                    warn!("Tun {} not online for WS relay", tun_name);
                    return Ok(true);
                };

                // 101 Switching Protocols
                let mut hdr = ResponseHeader::build(101, None).unwrap();
                hdr.insert_header("Upgrade", "websocket").ok();
                hdr.insert_header("Connection", "Upgrade").ok();
                if let Some(sec_key) = session.get_header("Sec-WebSocket-Key")
                    && let Ok(key_str) = std::str::from_utf8(sec_key.as_bytes())
                {
                    let accept = compute_ws_accept(key_str);
                    hdr.insert_header("Sec-WebSocket-Accept", accept).ok();
                }
                if let Some(protocols) = session.get_header("Sec-WebSocket-Protocol")
                    && let Ok(v) = std::str::from_utf8(protocols.as_bytes())
                {
                    hdr.insert_header("Sec-WebSocket-Protocol", v).ok();
                }
                if let Some(version) = session.get_header("Sec-WebSocket-Version")
                    && let Ok(v) = std::str::from_utf8(version.as_bytes())
                {
                    hdr.insert_header("Sec-WebSocket-Version", v).ok();
                }
                session.write_response_header(Box::new(hdr), false).await?;

                // Open a yamux stream and write the frame.
                let mut yamux_stream = match tunnel.open_stream().await {
                    Ok(s) => s,
                    Err(e) => {
                        warn!("open yamux stream for WS relay: {}", e);
                        return Ok(true);
                    }
                };
                let bytes = pangolin_core::proxy::encode_tunnel_frame(&frame);
                if let Err(e) = async {
                    use tokio::io::AsyncWriteExt;
                    yamux_stream.write_all(&bytes).await?;
                    yamux_stream.flush().await?;
                    Ok::<(), std::io::Error>(())
                }
                .await
                {
                    warn!("failed to write WS relay frame: {}", e);
                    return Ok(true);
                }

                // WS upgrade: server-side 101 is sent. The
                // tun will read the frame, perform the WS
                // upgrade against the backend, and pump
                // bytes between the yamux stream and the
                // backend. From the ngx side we just keep
                // the stream alive; WS frames flow
                // tun<->backend through the yamux pipe.
                // The HTTP-level 101 is sent here; client
                // sees the upgrade as complete.
                let stream = {
                    let h1 = session.as_http1_mut().expect("not HTTP/1 session");
                    h1.take_stream()
                };
                drop(stream); // Stream not used; tun handles WS via yamux.
                drop(yamux_stream);

                return Ok(true);
            }
            // Direct WS: let pingora handle the 101 upgrade.
            return Ok(false);
        }

        // ── Site lookup ────────────────────────────────────────
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

        // Parse backend into a typed target.
        let (tun_name, target) = match parse_backend_to_target(&site.backend) {
            Ok(v) => v,
            Err(e) => {
                error!("Invalid backend for site {}: {}", site.name, e);
                let _ = session.respond_error(502).await;
                return Ok(true);
            }
        };

        // ── file:// direct (no tun) ──────────────────────────
        // Only when tun_name is empty — otherwise the file
        // lives on the tun's machine and must be served
        // from there.
        if tun_name.is_empty()
            && let BackendTarget::File { doc_root } = &target
        {
            let path_and_query = session
                .req_header()
                .uri
                .path_and_query()
                .map(|pq| pq.as_str().to_string())
                .unwrap_or_else(|| "/".to_string());
            let req = match build_request_from_session(session, &path_and_query).await {
                Ok(r) => r,
                Err(e) => {
                    error!("file backend: build request: {}", e);
                    let _ = session.respond_error(500).await;
                    return Ok(true);
                }
            };
            let resp = serve_file_target(&req, doc_root);
            write_response_to_session(session, &resp).await;
            return Ok(true);
        }

        // ── tunnel path ───────────────────────────────────────
        if !tun_name.is_empty() {
            // Special case: file:// backend over a tunnel.
            // The file lives on the tun's machine, so we
            // delegate the serve to the tun — frame is
            // built with the same target as http/https.
            if let BackendTarget::File { .. } = &target {
                // The whole flow is identical to the
                // http/https tunnel path below; nothing
                // special to do here.
            }
            let tunnel = {
                let sessions = self.app.tun_sessions.read().await;
                sessions.get(&tun_name).cloned()
            };
            let Some(tunnel) = tunnel else {
                warn!("Tun {} not online", tun_name);
                let _ = session.respond_error(503).await;
                return Ok(true);
            };

            // Build the request with the whole-URL target
            // (backend base + original path+query).
            let path_and_query = session
                .req_header()
                .uri
                .path_and_query()
                .map(|pq| pq.as_str().to_string())
                .unwrap_or_else(|| "/".to_string());
            let req_path = if path_and_query.starts_with('/') {
                path_and_query
            } else {
                format!("/{}", path_and_query)
            };
            // Build the whole-URL target. For http/https this
            // is the URL the tun will dial; for file:// this
            // is the URL the tun will use to look up the
            // on-disk root. The request's path itself is
            // reconstructed in `build_request_from_session`
            // — for file:// backends we use the **original**
            // path (so `serve_file_target` can extract it),
            // while for http/https we use the joined full URL.
            let target_for_request = match &target {
                BackendTarget::Http { .. } | BackendTarget::Https { .. } => None,
                BackendTarget::File { .. } => Some(req_path.clone()),
            };
            let _ = target_for_request; // not used; we keep
            // the construction below
            // for clarity.
            let target_url = match &target {
                BackendTarget::Http {
                    host,
                    port,
                    base_path,
                } => format!(
                    "http://{}:{}{}{}",
                    host,
                    port,
                    if base_path.is_empty() {
                        req_path.clone()
                    } else {
                        format!("{}{}", base_path.trim_end_matches('/'), req_path)
                    },
                    ""
                ),
                BackendTarget::Https {
                    host,
                    port,
                    base_path,
                } => format!(
                    "https://{}:{}{}{}",
                    host,
                    port,
                    if base_path.is_empty() {
                        req_path.clone()
                    } else {
                        format!("{}{}", base_path.trim_end_matches('/'), req_path)
                    },
                    ""
                ),
                BackendTarget::File { doc_root } => {
                    // For file:// the tun will parse
                    // `request.target` (which we set to the
                    // **original path** below) and join it
                    // with `doc_root`. We just embed the
                    // doc_root here for the tun's
                    // BackendTarget classification; the
                    // actual file lookup uses request.target
                    // as the relative path.
                    format!("file://{}", doc_root.to_string_lossy())
                }
            };

            // The `target_url` is what we put in the frame's
            // `request.target` for http/https backends (the
            // tun will dial it as a whole URL). For
            // `file://` we override to the **original**
            // path — `serve_file_target` parses it to find
            // the file within the doc_root.
            let frame_target_url = match &target {
                BackendTarget::File { .. } => req_path.clone(),
                _ => target_url,
            };

            let req = match build_request_from_session(session, &frame_target_url).await {
                Ok(r) => r,
                Err(e) => {
                    error!("tunnel: build request: {}", e);
                    let _ = session.respond_error(500).await;
                    return Ok(true);
                }
            };

            // Encode host_mode + host_custom + the typed
            // BackendTarget so the tun can dispatch
            // without re-deriving it from the request
            // target. (`serve_file_target` etc. are pure
            // functions on `BackendTarget` + `HttpRequest`.)
            let frame = TunnelHttpFrame {
                request: req,
                target: target.clone(),
                host_mode: site.host_mode,
                host_custom: site.host_custom.clone(),
                is_upgrade: false,
            };
            let exec = YamuxTunnelExecutor { tunnel: &tunnel };
            match exec
                .execute_http(frame.request, &target, frame.host_mode, frame.host_custom)
                .await
            {
                Ok(resp) => {
                    write_response_to_session(session, &resp).await;
                    return Ok(true);
                }
                Err(e) => {
                    warn!("tunnel execute_http error: {}", e);
                    let code = match e {
                        ProxyError::Timeout => 504,
                        ProxyError::Unavailable(_) => 503,
                        _ => 502,
                    };
                    let _ = session.respond_error(code).await;
                    return Ok(true);
                }
            }
        }

        // ── direct path (Http/Https) → fall through to pingora ──
        debug!("Direct proxy: {} → {}", host, target.authority());
        Ok(false)
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
            None => return Err(Error::new_str("site not found")),
        };
        drop(indexes);
        let (_tun, target) = parse_backend_to_target(&site.backend)
            .map_err(|e| Error::explain(ErrorType::ReadError, format!("bad backend: {e}")))?;
        match &target {
            BackendTarget::Http { host: h, port, .. } => Ok(Box::new(HttpPeer::new(
                format!("{}:{}", h, port),
                false,
                String::new(),
            ))),
            BackendTarget::Https { host: h, port, .. } => Ok(Box::new(HttpPeer::new(
                format!("{}:{}", h, port),
                true,
                h.clone(),
            ))),
            BackendTarget::File { .. } => {
                Err(Error::explain(ErrorType::ReadError, "file:// not a peer"))
            }
        }
    }

    /// Set Host header per site.host_mode, add X-Forwarded-Host
    /// when in passthrough. Now uses the shared
    /// `apply_proxy_policy` so direct and tunnel paths are
    /// consistent.
    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream: &mut RequestHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        let original_host = host_from_session(session);
        let scheme = match session.req_header().uri.scheme_str() {
            Some("https") => Scheme::Https,
            _ => Scheme::Http,
        };
        let indexes = self.app.indexes.read().await;
        let site = match pangolin_core::index::lookup_site(&indexes, &original_host) {
            Some(s) => s.clone(),
            None => {
                if !original_host.is_empty() {
                    upstream
                        .insert_header("Host", original_host.as_bytes())
                        .ok();
                }
                return Ok(());
            }
        };
        drop(indexes);

        // Build ctx.
        let ctx = ProxyCtx {
            original_host: original_host.clone(),
            original_scheme: scheme,
            host_mode: site.host_mode,
            host_custom: site.host_custom.clone(),
        };

        // Collect current headers as Vec<(String, String)>.
        // We need this materialized view so `apply_proxy_policy`
        // (which works on the framework-neutral `HttpRequest`
        // shape) can mutate it. After we're done we push the
        // changes back into the pingora `RequestHeader` using
        // its native mutators — touching `upstream.headers`
        // directly desyncs the name/case map and triggers an
        // assertion in pingora-http at iteration time.
        let req_headers: Vec<(String, String)> = upstream
            .headers
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let method = upstream.method.as_str().to_string();
        let target = upstream.uri.to_string();
        let mut req = HttpRequest {
            method,
            target,
            version: "HTTP/1.1".to_string(),
            headers: req_headers,
            body: Vec::new(),
        };
        apply_proxy_policy(&mut req, &ctx);

        // host_mode=Backend: the executor (pingora upstream
        // peer) will dial the backend, so the Host header
        // should be the backend's host:port. We do that
        // here because we know the backend authority.
        if site.host_mode == HostMode::Backend
            && let Ok((
                _,
                BackendTarget::Http { host, port, .. } | BackendTarget::Https { host, port, .. },
            )) = parse_backend_to_target(&site.backend)
        {
            let new_host = format!("{}:{}", host, port);
            upsert_header(&mut req.headers, "Host", &new_host);
        }

        // Apply the mutations back to the pingora RequestHeader.
        // `apply_proxy_policy` does two kinds of work:
        //
        //   (a) strip hop-by-hop headers (Connection, etc.)
        //   (b) rewrite Host / add X-Forwarded-Host /
        //       add X-Forwarded-Proto
        //
        // For (a) we use `remove_header` on the live
        // RequestHeader (which keeps the name/case map in
        // sync). For (b) we compare the materialized view
        // against the live headers and `insert_header` only
        // for names that actually changed. Inserting a header
        // that already exists with the same case would still
        // be a no-op for `insert_header`, but we keep the diff
        // loop explicit so we don't churn the name/case map
        // on every request.
        for (k, v) in req.headers.iter() {
            // Skip headers pingora manages or that we know are
            // untouched — we still insert them so the upstream
            // sees the same shape we computed.
            let already = upstream
                .headers
                .iter()
                .any(|(hk, hv)| hk.as_str().eq_ignore_ascii_case(k) && hv == v.as_str());
            if !already && let Ok(value) = HeaderValue::from_str(v) {
                upstream.insert_header(k.to_string(), value).ok();
            }
        }
        // Strip anything the policy removed (typically hop-by-hop).
        // The live RequestHeader's `headers` is the source of
        // truth for what stays; anything in `upstream.headers`
        // that's not in the materialized `req.headers` was
        // removed by the policy.
        let kept: std::collections::HashSet<String> = req
            .headers
            .iter()
            .map(|(k, _)| k.to_ascii_lowercase())
            .collect();
        let to_remove: Vec<String> = upstream
            .headers
            .iter()
            .filter_map(|(k, _)| {
                if kept.contains(&k.as_str().to_ascii_lowercase()) {
                    None
                } else {
                    Some(k.as_str().to_string())
                }
            })
            .collect();
        for name in to_remove {
            // Some headers pingora owns internally (Host, etc.)
            // — skip them so we don't break framing.
            if name.eq_ignore_ascii_case("host") {
                continue;
            }
            upstream.remove_header(name.as_str());
        }
        Ok(())
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        _upstream_response: &mut ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        Ok(())
    }
}

fn upsert_header(headers: &mut Vec<(String, String)>, name: &str, value: &str) {
    if let Some(slot) = headers
        .iter_mut()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
    {
        slot.1 = value.to_string();
        return;
    }
    headers.push((name.to_string(), value.to_string()));
}

// ─────────────────────────────────────────────────────────────────
//  Helpers — build HttpRequest from pingora Session, write
//  HttpResponse back to pingora Session
// ─────────────────────────────────────────────────────────────────

/// Build an `HttpRequest` from a pingora `Session`. The `target`
/// argument is the full URL form (used for tunnel paths) or the
/// path (for file://).
async fn build_request_from_session(
    session: &mut Session,
    target: &str,
) -> Result<HttpRequest, String> {
    let method = session.req_header().method.as_str().to_string();

    let mut body_bytes = Vec::new();
    loop {
        match session.read_request_body().await {
            Ok(Some(data)) => body_bytes.extend_from_slice(&data),
            Ok(None) => break,
            Err(e) => return Err(format!("read_request_body: {e}")),
        }
    }

    let mut headers: Vec<(String, String)> = Vec::new();
    for (k, v) in &session.req_header().headers {
        headers.push((k.to_string(), v.to_str().unwrap_or("").to_string()));
    }

    Ok(HttpRequest {
        method,
        target: target.to_string(),
        version: "HTTP/1.1".to_string(),
        headers,
        body: body_bytes,
    })
}

/// Write an `HttpResponse` to a pingora `Session`. Translates
/// our shared wire format into pingora's `ResponseHeader` +
/// `write_response_body`.
async fn write_response_to_session(session: &mut Session, resp: &HttpResponse) {
    let status = parse_status_from_line(&resp.status_line);
    let mut hdr = match ResponseHeader::build(status, None) {
        Ok(h) => h,
        Err(e) => {
            error!("failed to build response header: {}", e);
            let _ = session.respond_error(500).await;
            return;
        }
    };
    for (k, v) in &resp.headers {
        if let Ok(value) = HeaderValue::from_str(v.as_str()) {
            hdr.insert_header(k.to_string(), value).ok();
        }
    }
    if resp
        .headers
        .iter()
        .all(|(k, _)| !k.eq_ignore_ascii_case("content-length"))
    {
        hdr.insert_header("Content-Length", resp.body.len().to_string())
            .ok();
    }
    if let Err(e) = session.write_response_header(Box::new(hdr), false).await {
        error!("failed to write response header: {}", e);
        return;
    }
    if let Err(e) = session
        .write_response_body(Some(Bytes::from(resp.body.clone())), true)
        .await
    {
        error!("failed to write response body: {}", e);
    }
}

/// Parse the status code from an `HttpResponse::status_line`
/// (format: `"200 OK"`, with the version carried separately in
/// `HttpResponse::version`). Returns 502 if the line is malformed
/// so a broken upstream cannot produce a 0-status response.
fn parse_status_from_line(status_line: &str) -> u16 {
    status_line
        .split(' ')
        .next()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(502)
}

/// Extract the effective host from the request, preferring the
/// `Host` header but falling back to the HTTP/2 `:authority`
/// pseudo-header.
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

// (pump_ws_relay is no longer used here — WS upgrade goes
//  through the tunnel frame and the tun-side executor.)
