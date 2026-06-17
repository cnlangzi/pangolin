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

use pangolin_core::decode_http_response;
use pangolin_core::tunnel::{HttpRequest, HttpResponse, compute_ws_accept};
use pangolin_core::types::HostMode;

use pangolin_core::{
    BackendTarget, ProxyCtx, Scheme, TunnelHttpFrame, apply_proxy_policy,
    apply_proxy_policy_without_hop_by_hop_stripping, parse_backend_to_target, serve_file_target,
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
/// is_upgrade, is_streaming) and waits for the matching
/// response.
///
/// `is_streaming` is currently only used as a **routing flag**
/// on the tun side: when set, the tun dispatches to
/// `handle_streaming_response` (byte-relay path) instead of
/// `handle_http_request` (buffered `HttpResponse` path). The
/// executor still reads back a full `HttpResponse` from the
/// yamux stream, because the byte-relay path on the tun side
/// terminates the stream cleanly after the response — the
/// protocol surface is unchanged.
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
        // The buffered tunnel executor never carries a streaming
        // request — the SSE / streaming path short-circuits in
        // `request_filter` and constructs its own frame directly
        // with `is_streaming = true`. Hardcoding `false` here
        // avoids a per-request header scan for a value that is
        // provably always false on this code path.
        let frame = TunnelHttpFrame {
            request,
            target: target.clone(),
            host_mode,
            host_custom,
            is_upgrade,
            is_streaming: false,
        };
        let bytes = pangolin_core::proxy::encode_tunnel_frame(&frame);

        let mut stream = self
            .tunnel
            .open_stream()
            .await
            .map_err(|e| ProxyError::Unavailable(format!("open_stream: {e}")))?;
        // Write 4-byte big-endian length prefix followed by frame bytes.
        // The tun side reads the length first, then reads exactly that many
        // bytes — no need to wait for EOF (which is unreliable over yamux).
        let len = bytes.len() as u32;
        stream
            .write_all(&len.to_be_bytes())
            .await
            .map_err(|e| ProxyError::Backend(format!("write frame length: {e}")))?;
        stream
            .write_all(&bytes)
            .await
            .map_err(|e| ProxyError::Backend(format!("write frame: {e}")))?;
        stream
            .flush()
            .await
            .map_err(|e| ProxyError::Backend(format!("flush: {e}")))?;

        // Read 4-byte big-endian length prefix, then read exactly that many
        // bytes for the response. Mirrors the tun→ngx write protocol.
        use tokio::io::AsyncReadExt as _;
        let mut len_buf = [0u8; 4];
        timeout(Duration::from_secs(60), stream.read_exact(&mut len_buf))
            .await
            .map_err(|_| ProxyError::Timeout)?
            .map_err(|e| ProxyError::Backend(format!("read response length: {e}")))?;
        let resp_len = u32::from_be_bytes(len_buf) as usize;
        let mut resp_buf = vec![0u8; resp_len];
        timeout(Duration::from_secs(60), stream.read_exact(&mut resp_buf))
            .await
            .map_err(|_| ProxyError::Timeout)?
            .map_err(|e| ProxyError::Backend(format!("read response body: {e}")))?;

        let response = decode_http_response(&resp_buf)
            .map_err(|e| ProxyError::Backend(format!("decode response: {e}")))?
            .ok_or_else(|| ProxyError::Backend("empty response".into()))?;

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

/// Per-request state for access logging (Issue #73).
/// Captures start time, method, path, host, and backend so
/// response_filter can construct an AccessLogEntry.
#[derive(Debug, Clone)]
pub struct RequestState {
    pub start: std::time::Instant,
    pub method: String,
    pub path: String,
    pub host: String,
    pub backend: String, // "tun:office" | "direct:1.2.3.4:8080" | "file://..."
}

impl Default for RequestState {
    fn default() -> Self {
        Self {
            start: std::time::Instant::now(),
            method: String::new(),
            path: String::new(),
            host: String::new(),
            backend: String::new(),
        }
    }
}

/// `ProxyHttp` implementation for pangolin.
pub struct AppProxy {
    pub app: Arc<App>,
}

#[async_trait]
impl ProxyHttp for AppProxy {
    type CTX = RequestState;

    fn new_ctx(&self) -> Self::CTX {
        RequestState::default()
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        let path = session.req_header().uri.path().to_string();
        let is_upgrade = session.is_upgrade_req();
        log::debug!(
            "PROXY: request_filter path={} is_ws_upgrade={}",
            path,
            is_upgrade
        );

        // Issue #73: capture request start time + basics for access log
        ctx.start = std::time::Instant::now();
        ctx.method = session.req_header().method.as_str().to_string();
        ctx.path = path.clone();
        ctx.host = host_from_session(session);

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

        // ── SSE / streaming response short-circuit ────────────
        // Detected up-front (before site lookup) so we can pick
        // the byte-relay path on both ends. The heuristic is
        // intentionally conservative: only `text/event-stream`
        // is matched today. Add new patterns to
        // `pangolin_core::is_streaming_request` to extend.
        //
        // NOTE: this iterates over *all* request headers (not
        // just the first Accept/Content-Type) so it matches
        // multi-value Accept headers that HTTP/2 clients may
        // send. Keep the semantics aligned with
        // `pangolin_core::is_streaming_request` — see
        // `is_streaming_request_detects_text_event_stream`.
        let is_streaming = session.req_header().headers.iter().any(|(k, v)| {
            let name = k.as_str();
            (name.eq_ignore_ascii_case("Accept") || name.eq_ignore_ascii_case("Content-Type"))
                && v.to_str()
                    .map(|s| s.to_ascii_lowercase().contains("text/event-stream"))
                    .unwrap_or(false)
        });

        if is_streaming {
            return handle_streaming_request(&self.app, session).await;
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
                    is_streaming: false,
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

        // Issue #73: record backend string for access log
        ctx.backend = if tun_name.is_empty() {
            format!("direct:{}", target.authority())
        } else {
            format!("tun:{}", tun_name)
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
            let mut req = match build_request_from_session(session, &path_and_query).await {
                Ok(r) => r,
                Err(e) => {
                    error!("file backend: build request: {}", e);
                    let _ = session.respond_error(500).await;
                    return Ok(true);
                }
            };
            // Apply the same proxy policy (hop-by-hop stripping,
            // X-Forwarded-*) the direct and tunnel paths do, so
            // the file-serve path stays consistent with the
            // v8 invariant of "all 18 combinations behave
            // identically."
            let original_host = host_from_session(session);
            let scheme = match session.req_header().uri.scheme_str() {
                Some("https") => Scheme::Https,
                _ => Scheme::Http,
            };
            let proxy_ctx = ProxyCtx {
                original_host,
                original_scheme: scheme,
                host_mode: site.host_mode,
                host_custom: site.host_custom.clone(),
            };
            apply_proxy_policy(&mut req, &proxy_ctx);
            let resp = serve_file_target(&req, doc_root);
            // Issue #73: file:// requests also short-circuit pingora
            // — record the access log here for parity with the
            // tunnel + direct paths. `ctx` (the trait's
            // `RequestState`) is unaffected because the proxy
            // policy `ProxyCtx` was renamed to `proxy_ctx` above.
            record_access_log(
                &self.app,
                ctx,
                session,
                parse_status_from_line(&resp.status_line),
            );
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
            let target_url = match &target {
                BackendTarget::Http {
                    host,
                    port,
                    base_path,
                } => format!(
                    "http://{}:{}{}",
                    host,
                    port,
                    if base_path.is_empty() {
                        req_path.clone()
                    } else {
                        format!("{}{}", base_path.trim_end_matches('/'), req_path)
                    },
                ),
                BackendTarget::Https {
                    host,
                    port,
                    base_path,
                } => format!(
                    "https://{}:{}{}",
                    host,
                    port,
                    if base_path.is_empty() {
                        req_path.clone()
                    } else {
                        format!("{}{}", base_path.trim_end_matches('/'), req_path)
                    },
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
                is_streaming: false,
            };
            let exec = YamuxTunnelExecutor { tunnel: &tunnel };
            match exec
                .execute_http(frame.request, &target, frame.host_mode, frame.host_custom)
                .await
            {
                Ok(resp) => {
                    // Issue #73: tunnel-path requests short-circuit
                    // pingora so the proxy `response_filter` never
                    // runs — record the access log here so the entry
                    // shows up on /logs.
                    record_access_log(
                        &self.app,
                        ctx,
                        session,
                        parse_status_from_line(&resp.status_line),
                    );
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
        // Use the variant without hop-by-hop stripping because
        // Pingora handles those automatically in its HTTP/1.1
        // serialization. Stripping them here would desync the
        // header_name_map, causing assertion failures.
        apply_proxy_policy_without_hop_by_hop_stripping(&mut req, &ctx);

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
        // Since we're using apply_proxy_policy_without_hop_by_hop_stripping,
        // we only need to handle:
        //   - Host header rewrites (for Backend/Custom modes)
        //   - X-Forwarded-* additions (for Backend/Custom modes)
        //
        // Pingora handles hop-by-hop headers automatically, so we
        // don't need to remove them here.
        for (k, v) in req.headers.iter() {
            let already = upstream
                .headers
                .iter()
                .any(|(hk, hv)| hk.as_str().eq_ignore_ascii_case(k) && hv == v.as_str());
            // `append_header` (not `insert_header`) for the same reason as
            // `write_response_to_session`: a client request may legitimately
            // carry multiple headers with the same name (e.g. several
            // `Forwarded` entries per RFC 7239 §4, or a `Via` chain), and
            // `insert_header` would silently drop every value after the first.
            // The `already` guard above suppresses *exact* (name, value)
            // duplicates so we don't accidentally double an unchanged header.
            if !already && let Ok(value) = HeaderValue::from_str(v) {
                upstream.append_header(k.to_string(), value).ok();
            }
        }
        Ok(())
    }

    async fn response_filter(
        &self,
        session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        // Issue #73: construct AccessLogEntry and push to App
        let status = upstream_response.status.as_u16();
        record_access_log(&self.app, ctx, session, status);
        Ok(())
    }
}

/// Handle a streaming (SSE / long-lived chunked) request over
/// the tunnel.
///
/// Mirrors the WebSocket relay at the top of `request_filter`:
///   1. Site lookup → determine the tun name + backend URL.
///   2. Open a yamux stream to the tun.
///   3. Write the `TunnelHttpFrame` (with `is_streaming = true`).
///   4. Read the response **header** off the yamux stream —
///      just the HTTP head (status line + headers), no body
///      length required because we are going to stream the
///      body. The tun side writes the head, then the body
///      bytes are relayed verbatim from the backend TCP
///      socket by `handle_streaming_response` in the tun
///      binary.
///   5. Send the response head to the client, then
///      `copy_bidirectional` between the yamux stream and
///      the client session until either side closes.
///
/// This mirrors what `handle_ws_upgrade` does in the tun:
/// both sides treat the yamux stream as a transparent byte
/// pipe once the handshake is done.
async fn handle_streaming_request(app: &App, session: &mut Session) -> Result<bool> {
    let host = host_from_session(session);

    let indexes = app.indexes.read().await;
    let site = match pangolin_core::index::lookup_site(&indexes, &host) {
        Some(s) => s.clone(),
        None => {
            debug!("SSE: no site for host {}", host);
            let _ = session.respond_error(404).await;
            return Ok(true);
        }
    };
    drop(indexes);

    let (tun_name, target) = match parse_backend_to_target(&site.backend) {
        Ok(v) => v,
        Err(e) => {
            error!("SSE: invalid backend for {}: {}", site.name, e);
            let _ = session.respond_error(502).await;
            return Ok(true);
        }
    };

    if tun_name.is_empty() {
        // Streaming is a tunnel-only feature for now — direct
        // backends should still work, but the buffering path
        // (which we explicitly bypassed up front) needs a
        // non-tunnel implementation. We do not have one yet;
        // return 501 to surface that the feature is
        // tunnel-only.
        warn!(
            "SSE: no tun configured for {} (streaming is tunnel-only)",
            host
        );
        let _ = session.respond_error(501).await;
        return Ok(true);
    }

    if matches!(target, BackendTarget::File { .. }) {
        // File:// backends over a tunnel are technically
        // supported (the tun can serve files), but streaming
        // makes no sense for a single-file response. Bail.
        let _ = session.respond_error(400).await;
        return Ok(true);
    }

    let tunnel = {
        let sessions = app.tun_sessions.read().await;
        sessions.get(&tun_name).cloned()
    };
    let Some(tunnel) = tunnel else {
        warn!("SSE: tun {} not online", tun_name);
        let _ = session.respond_error(503).await;
        return Ok(true);
    };

    info!("Tunnel SSE relay: {} → tun {}", host, tun_name);

    // Build the request — same URL construction as the buffered
    // tunnel path, so path-prefix joining and host header rules
    // are identical.
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

    // For the frame, the tun reads `request.target` to find
    // the URL it must dial. We follow the same convention as
    // the buffered tunnel path: full URL for http/https, the
    // original path for file://.
    let req_target = match &target {
        BackendTarget::File { .. } => req_path.clone(),
        BackendTarget::Http {
            host,
            port,
            base_path,
        } => format!(
            "http://{}:{}{}",
            host,
            port,
            if base_path.is_empty() {
                req_path.clone()
            } else {
                format!("{}{}", base_path.trim_end_matches('/'), req_path)
            }
        ),
        BackendTarget::Https {
            host,
            port,
            base_path,
        } => format!(
            "https://{}:{}{}",
            host,
            port,
            if base_path.is_empty() {
                req_path.clone()
            } else {
                format!("{}{}", base_path.trim_end_matches('/'), req_path)
            }
        ),
    };

    let req = match build_request_from_session(session, &req_target).await {
        Ok(r) => r,
        Err(e) => {
            error!("SSE: build request: {}", e);
            let _ = session.respond_error(500).await;
            return Ok(true);
        }
    };

    let frame = TunnelHttpFrame {
        request: req,
        target: target.clone(),
        host_mode: site.host_mode,
        host_custom: site.host_custom.clone(),
        is_upgrade: false,
        is_streaming: true,
    };

    let mut yamux_stream = match tunnel.open_stream().await {
        Ok(s) => s,
        Err(e) => {
            warn!("SSE: open yamux stream: {}", e);
            let _ = session.respond_error(502).await;
            return Ok(true);
        }
    };

    // Write the frame (length-prefixed). Same wire format as
    // the buffered path.
    let bytes = pangolin_core::proxy::encode_tunnel_frame(&frame);
    let len = bytes.len() as u32;
    if let Err(e) = async {
        use tokio::io::AsyncWriteExt;
        yamux_stream.write_all(&len.to_be_bytes()).await?;
        yamux_stream.write_all(&bytes).await?;
        yamux_stream.flush().await?;
        Ok::<(), std::io::Error>(())
    }
    .await
    {
        warn!("SSE: write frame: {}", e);
        let _ = session.respond_error(502).await;
        return Ok(true);
    }

    // Read the response head off the yamux stream. The tun
    // writes the head as soon as the backend has produced
    // it, then relays body bytes. We do not require a length
    // prefix here: we just scan for the CRLFCRLF delimiter.
    use tokio::io::AsyncReadExt;
    let mut header_buf: Vec<u8> = Vec::with_capacity(2048);
    let mut tmp = [0u8; 1024];
    let header_read = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let n = yamux_stream.read(&mut tmp).await?;
            if n == 0 {
                return Err::<usize, std::io::Error>(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "tun closed before sending response head",
                ));
            }
            header_buf.extend_from_slice(&tmp[..n]);
            if let Some(idx) = header_buf.windows(4).position(|w| w == b"\r\n\r\n") {
                return Ok(idx + 4);
            }
            if header_buf.len() > 64 * 1024 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "response head >64 KiB without CRLFCRLF",
                ));
            }
        }
    })
    .await;
    let head_end = match header_read {
        Ok(Ok(n)) => n,
        Ok(Err(e)) => {
            warn!("SSE: read response head: {}", e);
            let _ = session.respond_error(502).await;
            return Ok(true);
        }
        Err(_) => {
            warn!("SSE: read response head timeout (60s)");
            let _ = session.respond_error(504).await;
            return Ok(true);
        }
    };

    // Preserve any body bytes that came along with the head
    // read — yamux is a streaming transport, so a single `read`
    // often returns the entire head plus the first body chunk
    // in one go. Discarding those bytes silently drops the
    // first SSE event (the test panic on CI showed exactly
    // this: events 1 and 2 arrived but event 0 was missing).
    let pre_buffered_body: Vec<u8> = header_buf[head_end..].to_vec();

    // Parse the status line + headers. We need a ResponseHeader
    // that pingora can write to the client.
    let head_str = match std::str::from_utf8(&header_buf[..head_end]) {
        Ok(s) => s,
        Err(e) => {
            warn!("SSE: response head is not valid UTF-8: {}", e);
            let _ = session.respond_error(502).await;
            return Ok(true);
        }
    };
    let mut status: u16 = 502;
    let mut resp_headers: Vec<(String, String)> = Vec::new();
    {
        let mut lines = head_str.split("\r\n");
        if let Some(status_line) = lines.next() {
            // "HTTP/1.1 200 OK"
            let mut parts = status_line.splitn(3, ' ');
            let _ = parts.next();
            if let Some(code) = parts.next() {
                status = code.parse().unwrap_or(502);
            }
        }
        for line in lines {
            if line.is_empty() {
                continue;
            }
            if let Some((k, v)) = line.split_once(':') {
                resp_headers.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
    }

    let mut hdr = match ResponseHeader::build(status, None) {
        Ok(h) => h,
        Err(e) => {
            error!("SSE: build response header: {}", e);
            let _ = session.respond_error(502).await;
            return Ok(true);
        }
    };
    // `append_header` is the multi-value-safe variant (see
    // `real_e2e_tunnel_preserves_multiple_set_cookie`). It takes
    // owned `String` values, so we hand it clones of the (k, v)
    // pairs directly — the borrow on `resp_headers` only needs
    // to last for the iteration, not for the call.
    for (k, v) in &resp_headers {
        let _ = hdr.append_header(k.clone(), v.clone());
    }
    if let Err(e) = session.write_response_header(Box::new(hdr), false).await {
        error!("SSE: write response header: {}", e);
        return Ok(true);
    }

    // Drain any body bytes that were already in the head-read
    // buffer before we resume reading from the yamux stream.
    if !pre_buffered_body.is_empty()
        && let Err(e) = session
            .write_response_body(Some(Bytes::copy_from_slice(&pre_buffered_body)), false)
            .await
    {
        debug!("SSE: client closed early (prebuffer): {}", e);
        return Ok(true);
    }

    // Stream body bytes: yamux → client. We do not await
    // EOF — the next "tun wrote nothing for a while" or
    // client-close triggers a graceful return.
    let mut buf = [0u8; 8192];
    loop {
        match yamux_stream.read(&mut buf).await {
            Ok(0) => break, // tun closed the stream
            Ok(n) => {
                if let Err(e) = session
                    .write_response_body(Some(Bytes::copy_from_slice(&buf[..n])), false)
                    .await
                {
                    debug!("SSE: client closed early: {}", e);
                    break;
                }
            }
            Err(e) => {
                debug!("SSE: read from yamux: {}", e);
                break;
            }
        }
    }

    // Finalise the response body so the client sees the
    // body terminator / chunked-end / connection-close.
    if let Err(e) = session.finish_body().await {
        debug!("SSE: finish body: {}", e);
    }
    Ok(true)
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
///
/// **Why `append_header` (not `insert_header`)**: a backend may send
/// multiple headers under the same name — the canonical example is
/// `Set-Cookie` (RFC 6265 §3 / RFC 7230 §3.2.2 explicitly carves out
/// `Set-Cookie` as the one header where multi-line wire format is
/// meaningful). `insert_header` **replaces** all existing values
/// under the same name, silently dropping every `Set-Cookie` after
/// the first one. The user-visible symptom was a login flow where
/// only the last `Set-Cookie` reached the browser, the session
/// cookie got overwritten, and `/chat` immediately redirected back
/// to `/login`.
///
/// `append_header` adds the new value without removing existing
/// ones. For single-value headers like `Content-Length` it behaves
/// identically to `insert_header` for the common case (one value
/// in, one value out); if a misbehaving upstream sends duplicate
/// `Content-Length`, surfacing both to the client matches the raw
/// wire bytes the upstream sent — the right thing for a transparent
/// proxy.
///
/// See `real_e2e_tunnel_preserves_multiple_set_cookie`.
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
        if let Ok(value) = HeaderValue::from_str(v) {
            // We can't pass `k.as_str()` here even though it's cheaper
            // — pingora's `ResponseHeader::append_header` requires
            // `'static` for the name (its internal `IntoCaseHeaderName`
            // bound bottoms out in `bytes::Bytes`/`HeaderName`, which
            // both need owned storage).  `k.clone()` is the cheapest
            // path that satisfies the bound: a single short-string
            // allocation per header.
            //
            // `value` is already an owned `HeaderValue` from
            // `HeaderValue::from_str` above — no extra clone there.
            hdr.append_header(k.clone(), value).ok();
        }
    }
    // Only synthesise `Content-Length` when neither a `Content-Length` nor a
    // `Transfer-Encoding` header is already present.  RFC 7230 §3.3.3 forbids
    // both being set on the same message — some clients treat the combination
    // as a request-smuggling signal — and `encode_http_response` in
    // `pangolin-core` makes the same dual check.  Mirroring that guard keeps
    // the two code paths consistent for chunked upstream responses.
    let has_content_length = resp
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("content-length"));
    let has_transfer_encoding = resp
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("transfer-encoding"));
    if !has_content_length && !has_transfer_encoding {
        hdr.append_header("Content-Length", resp.body.len().to_string())
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

/// Build an [`AccessLogEntry`](pangolin_core::AccessLogEntry) from the
/// per-request [`RequestState`] and the client address from
/// `session`, and push it to [`App::push_access_log`]. Shared by
/// `response_filter` (direct / pingora path) and the tunnel +
/// file:// short-circuits in `request_filter` so every response
/// that the user actually sees is logged exactly once.
///
/// Tunnel and file paths in `request_filter` return `Ok(true)` to
/// short-circuit pingora, which means the proxy's `response_filter`
/// is never called for them — without this helper those requests
/// would silently disappear from the `/logs` page.
fn record_access_log(app: &Arc<App>, ctx: &RequestState, session: &Session, status: u16) {
    let duration_ms = ctx.start.elapsed().as_millis() as u64;
    let client_ip = session
        .client_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let entry = pangolin_core::AccessLogEntry {
        timestamp: chrono::Utc::now(),
        method: ctx.method.clone(),
        path: ctx.path.clone(),
        host: ctx.host.clone(),
        status,
        duration_ms,
        backend: ctx.backend.clone(),
        client_ip,
    };
    app.push_access_log(entry);
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
