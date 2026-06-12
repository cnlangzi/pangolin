//! Pangolin tunnel node (tun) — yamux-over-fastwebsockets client.
//!
//! Replaces the previous msgpack-framed WebSocket client with
//! a yamux multiplexer. Per-HTTP-request and per-WS-connection
//! a fresh yamux stream is opened against the ngx side; the
//! stream carries raw HTTP/1.1 bytes (or WS frame bytes for
//! relay) without any custom framing on top.
//!
//! ## Connection lifecycle
//!
//! 1. TCP-connect to the configured ngx address.
//! 2. Manual WS upgrade (RFC 6455), `Authorization: Bearer
//!    <sha256-of-token>` is sent in the upgrade request
//!    headers — auth happens before the 101 response.
//! 3. yamux client session on top of the resulting WS.
//! 4. A background task accepts new yamux streams from
//!    ngx, dispatching each to either the HTTP path
//!    (`handle_http_stream`) or the WS relay path
//!    (`handle_ws_stream`).
//! 5. On stream closure, drop the handle. On tunnel
//!    closure, the loop reconnects with backoff.

use std::time::Duration;

use anyhow::Result;
use log::{info, warn};

use pangolin_core::tunnel::{
    build_ws_upgrade_request, pump_ws_relay, read_http_request, read_ws_accept_response,
    tunnel_over_websocket, HttpRequest, TunnelRole, WsRole, YamuxTunnel,
};

use fastwebsockets::WebSocket;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::sleep;

use crate::config::TunConfig;

/// Outcome of a single connect + session cycle, used by
/// [`TunnelClient::run`] to drive the reconnect loop.
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
        // Client-side WS upgrade. The path includes the
        // tun name as a query parameter (the server uses it
        // to authorise against the `tun` table row; auth
        // token rides in the `Authorization: Bearer …`
        // header).
        let path = format!("/tunnel?name={}", self.config.name);
        let host = &self.config.server;
        let (req_bytes, key) = build_ws_upgrade_request(&path, host, &self.config.token);
        tcp_write.write_all(&req_bytes).await?;
        tcp_write.flush().await?;
        // Read the 101 response and validate the accept hash.
        read_ws_accept_response(&mut tcp_read, &key).await?;
        // Wrap the now-WS-framed halves as a WebSocket. We use
        // owned halves so the WebSocket can be moved into the
        // bridge task that yamux runs alongside.
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

/// Adapter so the split TCP halves can be reassembled into a
/// single `AsyncRead + AsyncWrite + Unpin` value for the
/// fastwebsockets `WebSocket::after_handshake` constructor.
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

        // Distinguish HTTP vs WS at the top byte of the first
        // yamux frame. We could add a 1-byte tag, but the
        // simplest disambiguation is to look at the first
        // line: `HTTP/1.1 …` is a response (we never receive
        // that here), `GET /… HTTP/1.1` is HTTP, and anything
        // else (binary frame from ngx) is WS. In practice, we
        // treat the first bytes as a discriminator:
        //
        //   - If the first 4 bytes are "GET " → HTTP request.
        //   - Otherwise → WS relay.
        //
        // The 1-byte tag is the simpler approach: ngx writes
        // `0x01` for HTTP, `0x02` for WS as the very first
        // byte of the stream. We do the same.
        let config = config.clone();
        let mut stream_obj = stream;
        let name_for_log = name.clone();
        tokio::spawn(async move {
            // Read the 1-byte tag.
            use tokio::io::AsyncReadExt;
            let mut tag = [0u8; 1];
            if stream_obj.read_exact(&mut tag).await.is_err() {
                return;
            }
            let mut tagged = TaggedStream {
                tag: tag[0],
                inner: stream_obj,
            };
            match tag[0] {
                TAG_HTTP => {
                    if let Err(e) = handle_http_stream(&mut tagged, &config, &name_for_log).await {
                        warn!("tun {} http stream error: {}", name_for_log, e);
                    }
                }
                TAG_WS => {
                    if let Err(e) = handle_ws_stream(&mut tagged, &config, &name_for_log).await {
                        warn!("tun {} ws stream error: {}", name_for_log, e);
                    }
                }
                other => {
                    warn!("tun {} unknown stream tag 0x{:02x}", name_for_log, other);
                }
            }
        });
    }
}

const TAG_HTTP: u8 = 0x01;
const TAG_WS: u8 = 0x02;

/// A yamux stream with a 1-byte tag already read off the
/// front. After consuming the tag, the rest of the stream
/// is the payload (raw HTTP request bytes, or raw WS frame
/// bytes).
struct TaggedStream<S> {
    #[allow(dead_code)]
    tag: u8,
    inner: S,
}

impl<S: AsyncRead + Unpin> AsyncRead for TaggedStream<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for TaggedStream<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Handle a single HTTP request stream from ngx. The flow:
///
///   1. Read the raw HTTP/1.1 request bytes (head + body)
///      off the stream.
///   2. Resolve the backend URL from the request target
///      (the issue spec has ngx writing the full URL into
///      the request target; a bare `/path` falls back to
///      the Host header).
///   3. Use reqwest to actually call the backend.
///   4. Re-serialise the response and write it back on
///      the stream.
async fn handle_http_stream<S>(stream: &mut S, config: &TunConfig, name: &str) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let req = read_http_request(stream).await?;
    info!("tun {} http {} {}", name, req.method, req.target);
    // For tun-side this is identical to before: the
    // request target is already a full URL (ngx builds it
    // from the site backend + request URI). Bare paths
    // are a legacy fallback using the Host header.
    let url = if req.target.starts_with("http://") || req.target.starts_with("https://") {
        req.target.clone()
    } else if req.target.starts_with("file:///") {
        return serve_static_file(stream, &req, &req.target, name).await;
    } else {
        let host = req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("Host"))
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| "127.0.0.1".to_string());
        format!("http://{}{}", host, req.target)
    };

    // Build and send the request via reqwest.
    let response_bytes = match proxy_via_reqwest(&req, &url, config).await {
        Ok(b) => b,
        Err(e) => {
            warn!("tun {} proxy error: {}", name, e);
            synth_502(&e.to_string())
        }
    };

    stream.write_all(&response_bytes).await?;
    stream.shutdown().await?;
    Ok(())
}

/// Handle a single WS relay stream from ngx. The flow:
///
///   1. Read the WS target URL from the first message
///      framed as a length-prefixed UTF-8 string.
///   2. Connect to the backend using a plain TCP stream
///      and do the WS upgrade manually — this gives us a
///      raw `AsyncRead + AsyncWrite` TCP stream carrying
///      WS frames.
///   3. Pump bytes bidirectionally between the TCP stream
///      and the yamux stream with the hand-written
///      half-close pump. The yamux stream itself carries
///      raw bytes that are WS frame payloads (one frame
///      per chunk); the bridge task inside YamuxTunnel
///      handles framing at the yamux side.
async fn handle_ws_stream<S>(stream: &mut S, _config: &TunConfig, name: &str) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // Length-prefixed URL: u16 BE + UTF-8 bytes.
    let mut len_buf = [0u8; 2];
    stream.read_exact(&mut len_buf).await?;
    let len = u16::from_be_bytes(len_buf) as usize;
    let mut url_buf = vec![0u8; len];
    stream.read_exact(&mut url_buf).await?;
    let backend_url = String::from_utf8_lossy(&url_buf).into_owned();
    info!("tun {} ws relay to {}", name, backend_url);

    let mut backend = TcpStream::connect(backend_addr_from_url(&backend_url)?).await?;
    let (req_bytes, key) = build_ws_upgrade_request(
        backend_path_from_url(&backend_url),
        backend_host_from_url(&backend_url),
        "",
    );
    backend.write_all(&req_bytes).await?;
    backend.flush().await?;
    read_ws_accept_response(&mut backend, &key).await?;

    pump_ws_relay(stream, backend, "ngx", "backend").await?;
    Ok(())
}

// ---- helpers ----

fn synth_502(err: &str) -> Vec<u8> {
    let body = err.as_bytes().to_vec();
    let mut out = Vec::new();
    out.extend_from_slice(b"HTTP/1.1 502 Bad Gateway\r\n");
    out.extend_from_slice(b"Content-Type: text/plain\r\n");
    out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    out.extend_from_slice(b"Connection: close\r\n");
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(&body);
    out
}

async fn proxy_via_reqwest(req: &HttpRequest, url: &str, _config: &TunConfig) -> Result<Vec<u8>> {
    use reqwest::header::{HeaderName, HeaderValue};
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(5))
        .pool_max_idle_per_host(16)
        .build()?;
    let parsed = url::Url::parse(url)?;
    let method = reqwest::Method::from_bytes(req.method.as_bytes())
        .map_err(|_| anyhow::anyhow!("invalid method"))?;
    let mut request = reqwest::Request::new(method, parsed);
    for (k, v) in &req.headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(v),
        ) {
            request.headers_mut().insert(name, value);
        }
    }
    if !req.body.is_empty() {
        *request.body_mut() = Some(reqwest::Body::from(req.body.clone()));
    }
    let resp = client.execute(request).await?;
    let status = resp.status();
    let mut out = Vec::new();
    out.extend_from_slice(
        format!(
            "HTTP/1.1 {} {}\r\n",
            status.as_u16(),
            status.canonical_reason().unwrap_or("")
        )
        .as_bytes(),
    );
    for (k, v) in resp.headers() {
        if let Ok(v) = v.to_str() {
            out.extend_from_slice(k.as_str().as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(v.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
    }
    if !out.windows(2).any(|w| w == b"\r\n") {
        // never empty — guaranteed by status line
    }
    let body = resp.bytes().await?.to_vec();
    out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(&body);
    Ok(out)
}

async fn serve_static_file<S>(
    stream: &mut S,
    _req: &HttpRequest,
    file_url: &str,
    _name: &str,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let path = file_url.strip_prefix("file://").unwrap_or(file_url);
    let resolved = if path.ends_with('/')
        || tokio::fs::metadata(path)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false)
    {
        let html = format!("{}/index.html", path.trim_end_matches('/'));
        if tokio::fs::metadata(&html).await.is_ok() {
            html
        } else {
            let htm = format!("{}/index.htm", path.trim_end_matches('/'));
            if tokio::fs::metadata(&htm).await.is_ok() {
                htm
            } else {
                return write_404(stream).await;
            }
        }
    } else {
        path.to_string()
    };
    match tokio::fs::read(&resolved).await {
        Ok(content) => {
            let mime = mime_guess::from_path(&resolved)
                .first_or_octet_stream()
                .to_string();
            let mut out = Vec::new();
            out.extend_from_slice(b"HTTP/1.1 200 OK\r\n");
            out.extend_from_slice(format!("Content-Type: {}\r\n", mime).as_bytes());
            out.extend_from_slice(format!("Content-Length: {}\r\n", content.len()).as_bytes());
            out.extend_from_slice(b"\r\n");
            out.extend_from_slice(&content);
            stream.write_all(&out).await?;
            stream.shutdown().await?;
            Ok(())
        }
        Err(_) => write_404(stream).await,
    }
}

async fn write_404<S: AsyncWrite + Unpin>(stream: &mut S) -> Result<()> {
    let body = b"Not Found".to_vec();
    let mut out = Vec::new();
    out.extend_from_slice(b"HTTP/1.1 404 Not Found\r\n");
    out.extend_from_slice(b"Content-Type: text/plain\r\n");
    out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(&body);
    stream.write_all(&out).await?;
    stream.shutdown().await?;
    Ok(())
}

fn backend_addr_from_url(url: &str) -> Result<std::net::SocketAddr> {
    // Strip scheme and path, leaving host:port.
    let after_scheme = url
        .trim_start_matches("ws://")
        .trim_start_matches("wss://")
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let host_port = after_scheme.split('/').next().unwrap_or(after_scheme);
    let addr: std::net::SocketAddr = host_port.parse()?;
    Ok(addr)
}

fn backend_host_from_url(url: &str) -> &str {
    let after_scheme = url
        .trim_start_matches("ws://")
        .trim_start_matches("wss://")
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    after_scheme.split('/').next().unwrap_or(after_scheme)
}

fn backend_path_from_url(url: &str) -> &str {
    let after_scheme = url
        .trim_start_matches("ws://")
        .trim_start_matches("wss://")
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    after_scheme
        .find('/')
        .map(|i| &after_scheme[i..])
        .unwrap_or("/")
}
