//! Pangolin tunnel node (tun) — yamux-over-fastwebsockets client.
//!
//! ## Architecture (v8 — see `docs/design/reverse-proxy.md`)
//!
//! Each yamux stream carries a [`TunnelHttpFrame`] from ngx
//! (see `pangolin-core::proxy`). The frame contains:
//!   - the raw `HttpRequest`
//!   - per-request `host_mode` and `host_custom`
//!   - `is_upgrade` flag (WebSocket upgrade)
//!
//! The tun applies `apply_proxy_policy` (pangolin-core) and
//! dispatches to one of three executors:
//!   - `file://` → `serve_file_target` (pangolin-core)
//!   - `http/https` → `PingoraClientExecutor` (this file)
//!   - WS upgrade → also via `PingoraClientExecutor`, which
//!     uses pingora's built-in upgrade path.
//!
//! There is **no reqwest** on this side: the v8 design uses
//! pingora-core for the HTTP client so behavior matches the
//! ngx side (single HTTP stack end-to-end).

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use log::{info, warn};

use pangolin_core::tunnel::{
    HttpRequest, HttpResponse, TunnelRole, WsRole, YamuxTunnel, build_ws_upgrade_request,
    encode_http_response, pump_ws_relay, read_ws_accept_response, tunnel_over_websocket,
};
use pangolin_core::types::HostMode;
use pangolin_core::{
    BackendTarget, ProxyCtx, Scheme, TunnelHttpFrame, apply_proxy_policy, serve_file_target,
};

use fastwebsockets::WebSocket;
use pingora_core::connectors::http::Connector;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_http::RequestHeader;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::sleep;

use crate::config::TunConfig;

// ─────────────────────────────────────────────────────────────────
//  WS handshake helpers (kept from the original file — used by
//  the connect path, not by the per-frame dispatch).
// ─────────────────────────────────────────────────────────────────

/// Outcome of a single connect + session cycle.
enum SessionOutcome {
    EstablishedAndEnded(Result<()>),
    NeverConnected(anyhow::Error),
}

pub struct TunnelClient {
    config: TunConfig,
}

impl TunnelClient {
    pub fn new(config: TunConfig) -> Self {
        Self { config }
    }

    pub async fn run(&self) {
        info!(
            "tun {} starting, target {}",
            self.config.name, self.config.server
        );

        const INITIAL_BACKOFF_SECS: u64 = 1;
        const MAX_BACKOFF_SECS: u64 = 30;
        const MAX_JITTER_MS: u64 = 500;

        let mut backoff_secs: u64 = INITIAL_BACKOFF_SECS;

        loop {
            let session_outcome = self.connect_and_handle().await;
            match session_outcome {
                SessionOutcome::EstablishedAndEnded(Ok(())) => {
                    info!("tun {} disconnected, will reconnect", self.config.name);
                    backoff_secs = INITIAL_BACKOFF_SECS;
                }
                SessionOutcome::EstablishedAndEnded(Err(e)) => {
                    warn!(
                        "tun {} session errored ({}), will reconnect",
                        self.config.name, e
                    );
                    backoff_secs = INITIAL_BACKOFF_SECS;
                }
                SessionOutcome::NeverConnected(e) => {
                    log::error!(
                        "tun {} connect error: {}, reconnecting in {}s",
                        self.config.name,
                        e,
                        backoff_secs
                    );
                }
            }

            let jitter_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_millis() as u64)
                .unwrap_or(0)
                % (MAX_JITTER_MS + 1);
            sleep(Duration::from_millis(backoff_secs * 1000 + jitter_ms)).await;
            backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
        }
    }

    async fn connect_and_handle(&self) -> SessionOutcome {
        let tcp = match TcpStream::connect(&self.config.server).await {
            Ok(s) => s,
            Err(e) => {
                return SessionOutcome::NeverConnected(anyhow::anyhow!(
                    "tcp connect to {}: {}",
                    self.config.server,
                    e
                ));
            }
        };
        info!(
            "tun {} connected to ngx {}, starting WS upgrade",
            self.config.name, self.config.server
        );
        match self.handshake_and_serve(tcp).await {
            Ok(()) => SessionOutcome::EstablishedAndEnded(Ok(())),
            Err(e) => SessionOutcome::EstablishedAndEnded(Err(e)),
        }
    }

    /// Perform the WebSocket upgrade against the connected TCP
    /// stream, hand the resulting WebSocket to a `YamuxTunnel`,
    /// and run the per-stream dispatcher until the tunnel ends.
    async fn handshake_and_serve(&self, tcp: TcpStream) -> Result<()> {
        let (mut tcp_read, mut tcp_write) = tcp.into_split();
        let path = format!("/tunnel?name={}", self.config.name);
        let host = &self.config.server;
        let (req_bytes, key) = build_ws_upgrade_request(&path, host, &self.config.token);
        tcp_write.write_all(&req_bytes).await?;
        tcp_write.flush().await?;
        read_ws_accept_response(&mut tcp_read, &key).await?;
        let ws = WebSocket::after_handshake(
            OwnedTcpHalf {
                reader: tcp_read,
                writer: tcp_write,
            },
            WsRole::Client,
        );
        let tunnel = tunnel_over_websocket(ws, TunnelRole::Client);
        info!("tun {} WS upgrade ok, yamux session live", self.config.name);
        serve_tun_session(&self.config, tunnel).await
    }
}

struct OwnedTcpHalf {
    reader: tokio::net::tcp::OwnedReadHalf,
    writer: tokio::net::tcp::OwnedWriteHalf,
}

impl AsyncRead for OwnedTcpHalf {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.reader).poll_read(cx, buf)
    }
}

impl AsyncWrite for OwnedTcpHalf {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.writer).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.writer).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.writer).poll_shutdown(cx)
    }
}

/// Run the tun-side per-stream dispatcher.
async fn serve_tun_session(config: &TunConfig, tunnel: YamuxTunnel) -> Result<()> {
    let name = config.name.clone();
    loop {
        let stream = match tunnel.accept_stream().await {
            Some(Ok(s)) => s,
            Some(Err(e)) => {
                warn!("tun {} accept error: {}", name, e);
                continue;
            }
            None => {
                info!("tun {} yamux session ended", name);
                return Ok(());
            }
        };

        let config = config.clone();
        let name_for_log = name.clone();
        tokio::spawn(async move {
            // v8: every yamux stream carries a single
            // `TunnelHttpFrame`. The previous TAG_HTTP /
            // TAG_WS discriminator bytes are removed —
            // see `pangolin_core::proxy`.
            if let Err(e) = handle_tunnel_frame(stream, &config, &name_for_log).await {
                warn!("tun {} tunnel frame error: {}", name_for_log, e);
            }
        });
    }
}

// ─────────────────────────────────────────────────────────────────
//  Per-frame dispatch
// ─────────────────────────────────────────────────────────────────

/// Read the `TunnelHttpFrame` bytes off the stream and hand
/// them to either `handle_http_request` or
/// `handle_ws_upgrade` based on the frame's `is_upgrade`
/// flag.
async fn handle_tunnel_frame<S>(mut stream: S, _config: &TunConfig, name: &str) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        match stream.read(&mut tmp).await {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e) => return Err(e.into()),
        }
    }
    let frame = decode_frame(&buf).map_err(|e| anyhow::anyhow!(e))?;
    if frame.is_upgrade {
        handle_ws_upgrade(stream, frame, name).await
    } else {
        handle_http_request(stream, frame, name).await
    }
}

/// Handle one HTTP request from a parsed `TunnelHttpFrame`:
///   1. Apply `apply_proxy_policy` (Host rewrite per
///      `host_mode`, X-Forwarded-*, hop-by-hop stripping).
///   2. Dispatch by target scheme:
///        - file:// → `serve_file_target` (pangolin-core)
///        - http/https → `execute_via_pingora`
///   3. Encode the response and write it back to the stream.
async fn handle_http_request<S>(mut stream: S, frame: TunnelHttpFrame, name: &str) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let original_host = frame
        .request
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Host"))
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let ctx = ProxyCtx {
        original_host,
        original_scheme: Scheme::Http,
        host_mode: frame.host_mode,
        host_custom: frame.host_custom.clone(),
    };

    info!(
        "tun {} http {} {} host_mode={:?}",
        name, frame.request.method, frame.request.target, ctx.host_mode
    );

    let mut request = frame.request;
    apply_proxy_policy(&mut request, &ctx);

    let response: HttpResponse = match &frame.target {
        BackendTarget::File { doc_root } => serve_file_target(&request, doc_root),
        target @ (BackendTarget::Http { .. } | BackendTarget::Https { .. }) => {
            let host_mode = ctx.host_mode;
            let host_custom = ctx.host_custom.clone();
            execute_via_pingora(&request, target, host_mode, host_custom)
                .await
                .map_err(|e| anyhow::anyhow!(e))?
        }
    };

    let resp_bytes = encode_http_response(&response);
    if let Err(e) = stream.write_all(&resp_bytes).await {
        warn!("tun {} write response: {}", name, e);
        return Err(e.into());
    }
    stream.shutdown().await?;
    Ok(())
}

/// Handle a WebSocket upgrade from a parsed `TunnelHttpFrame`.
/// Replays the request bytes to the backend (TCP + manual
/// WS handshake) and then pumps bytes between the yamux
/// stream and the backend.
async fn handle_ws_upgrade<S>(mut stream: S, frame: TunnelHttpFrame, name: &str) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let original_host = frame
        .request
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Host"))
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let ctx = ProxyCtx {
        original_host,
        original_scheme: Scheme::Http,
        host_mode: frame.host_mode,
        host_custom: frame.host_custom.clone(),
    };
    let mut request = frame.request;
    apply_proxy_policy(&mut request, &ctx);

    let (authority, path) = match request.target.find("://") {
        Some(idx) => {
            let rest = &request.target[idx + 3..];
            match rest.find('/') {
                Some(p) => (&rest[..p], &rest[p..]),
                None => (rest, "/"),
            }
        }
        None => {
            let resp_bytes = synth_502_bytes("ws target has no scheme");
            stream.write_all(&resp_bytes).await?;
            stream.shutdown().await?;
            return Ok(());
        }
    };

    let backend_addr: std::net::SocketAddr = match authority.parse() {
        Ok(a) => a,
        Err(e) => {
            let resp_bytes = synth_502_bytes(&format!("bad backend addr: {e}"));
            stream.write_all(&resp_bytes).await?;
            stream.shutdown().await?;
            return Ok(());
        }
    };

    info!("tun {} ws relay to {} path={}", name, backend_addr, path);

    let mut backend = TcpStream::connect(backend_addr).await?;
    let host_hdr = request
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Host"))
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| authority.to_string());
    let (req_bytes, key) = build_ws_upgrade_request(path, &host_hdr, "");
    backend.write_all(&req_bytes).await?;
    backend.flush().await?;
    read_ws_accept_response(&mut backend, &key).await?;
    pump_ws_relay(stream, backend, "ngx", "backend").await?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────
//  Frame codec
// ─────────────────────────────────────────────────────────────────

fn decode_frame(bytes: &[u8]) -> Result<TunnelHttpFrame, String> {
    pangolin_core::proxy::decode_tunnel_frame(bytes)
        .map_err(|e| format!("decode_tunnel_frame: {e}"))
}

// ─────────────────────────────────────────────────────────────────
//  PingoraClientExecutor — HTTP client using pingora-core
// ─────────────────────────────────────────────────────────────────

static CONNECTOR: std::sync::OnceLock<Arc<Connector>> = std::sync::OnceLock::new();

fn connector() -> &'static Arc<Connector> {
    CONNECTOR.get_or_init(|| Arc::new(Connector::new(None)))
}

/// Send `request` to the backend described by `target` using
/// pingora-core's HTTP/1.1 client. Honors `host_mode` (in
/// particular, Backend mode overwrites Host with the
/// backend's authority).
async fn execute_via_pingora(
    request: &HttpRequest,
    target: &BackendTarget,
    host_mode: HostMode,
    host_custom: Option<String>,
) -> Result<HttpResponse, String> {
    let peer = match target {
        BackendTarget::Http { host, port, .. } => {
            HttpPeer::new((host.clone(), *port), false, host.clone())
        }
        BackendTarget::Https { host, port, .. } => {
            HttpPeer::new((host.clone(), *port), true, host.clone())
        }
        BackendTarget::File { .. } => {
            return Err("file:// not handled by pingora client".into());
        }
    };

    let method =
        http::Method::from_bytes(request.method.as_bytes()).map_err(|e| format!("method: {e}"))?;

    // Path only.
    let path_only = match request.target.find("://") {
        Some(idx) => match request.target[idx + 3..].find('/') {
            Some(p) => &request.target[idx + 3 + p..],
            None => "/",
        },
        None => request.target.as_str(),
    };
    if path_only.is_empty() {
        return Err("empty path".into());
    }
    let path_static: &'static [u8] =
        Box::leak(path_only.to_string().into_bytes().into_boxed_slice());

    let mut header_builder = RequestHeader::build(&method, path_static, None)
        .map_err(|e| format!("build header: {e}"))?;

    // Insert all non-Host, non-Content-Length headers.
    let header_pairs: Vec<(String, String)> = request
        .headers
        .iter()
        .filter(|(k, _)| {
            !k.eq_ignore_ascii_case("Host") && !k.eq_ignore_ascii_case("Content-Length")
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for (k, v) in header_pairs {
        let name_static: &'static str = Box::leak(k.into_boxed_str());
        let _ = header_builder.insert_header(name_static, v.as_bytes());
    }

    // Apply host_mode Host override.
    let host_value = match host_mode {
        HostMode::Passthrough => request
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("Host"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default(),
        HostMode::Backend => match target {
            BackendTarget::Http { host, port, .. } | BackendTarget::Https { host, port, .. } => {
                format!("{}:{}", host, port)
            }
            _ => unreachable!(),
        },
        HostMode::Custom => host_custom.clone().unwrap_or_default(),
    };
    if !host_value.is_empty() {
        header_builder
            .insert_header("Host", host_value.as_bytes())
            .ok();
    }
    header_builder
        .insert_header("Content-Length", request.body.len().to_string().as_bytes())
        .ok();

    let connector = connector().clone();
    let (mut session, _reused) = connector
        .get_http_session(&peer)
        .await
        .map_err(|e| format!("get_http_session: {e}"))?;

    session
        .write_request_header(Box::new(header_builder))
        .await
        .map_err(|e| format!("write_request_header: {e}"))?;
    if !request.body.is_empty() {
        session
            .write_request_body(bytes::Bytes::copy_from_slice(&request.body), true)
            .await
            .map_err(|e| format!("write_request_body: {e}"))?;
    }
    session
        .finish_request_body()
        .await
        .map_err(|e| format!("finish_request_body: {e}"))?;

    session
        .read_response_header()
        .await
        .map_err(|e| format!("read_response_header: {e}"))?;

    let resp_header = session
        .response_header()
        .cloned()
        .ok_or_else(|| "no response header".to_string())?;

    let mut body = Vec::new();
    while let Some(chunk) = session
        .read_response_body()
        .await
        .map_err(|e| format!("read_response_body: {e}"))?
    {
        body.extend_from_slice(&chunk);
    }

    let idle = Some(Duration::from_secs(90));
    connector.release_http_session(session, &peer, idle).await;

    let status = resp_header.status;
    let status_line = format!(
        "{} {}",
        status.as_u16(),
        status.canonical_reason().unwrap_or("")
    );
    let mut headers: Vec<(String, String)> = Vec::new();
    for (k, v) in resp_header.headers.iter() {
        if let Ok(v) = v.to_str() {
            headers.push((k.as_str().to_string(), v.to_string()));
        }
    }
    Ok(HttpResponse {
        version: format!("{:?}", resp_header.version),
        status_line,
        headers,
        body,
    })
}

// ─────────────────────────────────────────────────────────────────
//  502 helpers
// ─────────────────────────────────────────────────────────────────

fn synth_502_bytes(err: &str) -> Vec<u8> {
    let body = err.as_bytes();
    let mut out = Vec::new();
    out.extend_from_slice(b"HTTP/1.1 502 Bad Gateway\r\n");
    out.extend_from_slice(b"Content-Type: text/plain\r\n");
    out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    out.extend_from_slice(b"Connection: close\r\n");
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}
