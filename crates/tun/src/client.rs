//! Pangolin tunnel node (tun) — WebSocket client implementation.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use log::warn;
use reqwest::{
    header::{HeaderName, HeaderValue},
    Client,
};
use tokio::sync::{mpsc, Mutex};
use tokio::time::{interval, sleep};
use tokio_tungstenite::{connect_async, tungstenite};

use crate::config::TunConfig;
use crate::frame::{
    deflate_decode, deflate_encode, deserialize_msgpack, serialize_msgpack, TunnelFrame,
    TunnelRequestFrame, TunnelResponseFrame,
};

struct PendingRequest;

/// Outcome of a single connect + session cycle, used by [`TunnelClient::run`]
/// to drive the reconnect loop. Distinguishing "we never got past the
/// handshake" from "we connected and then the session ended" is what
/// lets the loop reset the backoff only when a session actually ran.
enum SessionOutcome {
    /// WS handshake completed, then the session ended.
    /// `Ok(())` = clean stream close; `Err(e)` = session errored
    /// mid-flight (the underlying cause is logged, not surfaced).
    EstablishedAndEnded(Result<()>),
    /// Could not establish the WS connection at all (ngx offline,
    /// DNS failure, auth rejection, …).
    NeverConnected(anyhow::Error),
}

pub struct TunnelClient {
    config: TunConfig,
    http_client: Client,
    pending: Arc<Mutex<HashMap<String, PendingRequest>>>,
}

impl TunnelClient {
    pub fn new(config: TunConfig) -> Self {
        let http_client = Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(5))
            .pool_max_idle_per_host(16)
            .build()
            .expect("reqwest client should build");

        Self {
            config,
            http_client,
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn run(&self) {
        log::info!(
            "tun {} starting, target {}",
            self.config.name,
            self.config.server
        );

        // Reconnect policy:
        //   - Never exit on a dropped session. ngx may restart, the
        //     network may flap, or ngx may simply not be online yet
        //     (e.g. a tun that boots before its gateway). Any of those
        //     is recoverable; the operator controls lifecycle via
        //     systemd, not via the tun's idea of "done".
        //   - Exponential backoff with a 30s ceiling for connect
        //     failures (ngx offline).
        //   - Reset backoff to 1s after a session actually established
        //     (got past the WS handshake). A network blip that drops a
        //     healthy session should not leave us at 30s between every
        //     subsequent reconnect.
        //   - Small jitter (0..=500ms) so a fleet of tun clients
        //     reconnecting after an ngx restart doesn't synchronize
        //     their retries (thundering herd).
        const INITIAL_BACKOFF_SECS: u64 = 1;
        const MAX_BACKOFF_SECS: u64 = 30;
        const MAX_JITTER_MS: u64 = 500;

        let mut backoff_secs: u64 = INITIAL_BACKOFF_SECS;

        loop {
            let session_outcome = self.connect_and_handle().await;
            match session_outcome {
                SessionOutcome::EstablishedAndEnded(Ok(())) => {
                    log::info!("tun {} disconnected, will reconnect", self.config.name);
                    // We got past the handshake, so a session ran.
                    // Reset backoff — the next drop is a fresh event,
                    // not a continuation of an outage.
                    backoff_secs = INITIAL_BACKOFF_SECS;
                }
                SessionOutcome::EstablishedAndEnded(Err(e)) => {
                    log::warn!(
                        "tun {} session errored ({}), will reconnect",
                        self.config.name,
                        e
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
                    // Backoff not reset — keep doubling up to the cap.
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

    /// One connect+session cycle. Returns whether the WebSocket
    /// handshake ever completed, so the caller can decide whether
    /// to reset the reconnect backoff.
    async fn connect_and_handle(&self) -> SessionOutcome {
        let ws_url = format!(
            "ws://{}/tunnel?token={}&name={}",
            self.config.server, self.config.token, self.config.name
        );

        log::info!(
            "tun {} connecting to ws://{}/tunnel?token=***&name={}",
            self.config.name,
            self.config.server,
            self.config.name
        );
        let (ws_stream, _) = match connect_async(&ws_url).await {
            Ok(s) => s,
            Err(e) => return SessionOutcome::NeverConnected(e.into()),
        };
        log::info!("tun {} connected to ngx", self.config.name);

        match self.handle_stream(ws_stream).await {
            Ok(()) => SessionOutcome::EstablishedAndEnded(Ok(())),
            Err(e) => SessionOutcome::EstablishedAndEnded(Err(e)),
        }
    }

    async fn handle_stream(
        &self,
        ws: tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> Result<()> {
        let (ws_sender, mut ws_read) = ws.split();
        let pending = self.pending.clone();
        let name = self.config.name.clone();
        let http_client = self.http_client.clone();

        // Arc-wrapped sender so multiple concurrent request handlers can send
        let ws_sender = Arc::new(Mutex::new(ws_sender));

        // Batch channel: handle_request sends response frames here
        let (batch_tx, mut batch_rx) = mpsc::channel::<TunnelResponseFrame>(1000);

        // Batch flush task — collects frames for up to 10ms then sends in one WS write
        let ws_sender_batch = ws_sender.clone();
        let batch_handle = tokio::spawn(async move {
            const BATCH_DELAY_MS: u64 = 10;
            let mut batch: Vec<TunnelResponseFrame> = Vec::with_capacity(64);
            let mut flush_interval = tokio::time::interval(Duration::from_millis(BATCH_DELAY_MS));

            loop {
                tokio::select! {
                    Some(resp) = batch_rx.recv() => {
                        batch.push(resp);
                        // Flush immediately once batch reaches capacity
                        if batch.len() >= 64 {
                            for frame in batch.drain(..).map(TunnelFrame::Res) {
                                if let Ok(buf) = serialize_msgpack(&frame) {
                                    let compressed = deflate_encode(&buf);
                                    let mut sender = ws_sender_batch.lock().await;
                                    if sender.send(tungstenite::Message::Binary(compressed.into())).await.is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    _ = flush_interval.tick() => {
                        if batch.is_empty() {
                            continue;
                        }
                        for frame in batch.drain(..).map(TunnelFrame::Res) {
                            if let Ok(buf) = serialize_msgpack(&frame) {
                                let compressed = deflate_encode(&buf);
                                let mut sender = ws_sender_batch.lock().await;
                                if sender.send(tungstenite::Message::Binary(compressed.into())).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            // Drain remaining on exit
            if !batch.is_empty() {
                for frame in batch.drain(..).map(TunnelFrame::Res) {
                    if let Ok(buf) = serialize_msgpack(&frame) {
                        let compressed = deflate_encode(&buf);
                        let mut sender = ws_sender_batch.lock().await;
                        let _ = sender
                            .send(tungstenite::Message::Binary(compressed.into()))
                            .await;
                    }
                }
            }
        });

        // Keepalive ping task
        let ws_sender_ping = ws_sender.clone();
        let ping_handle = tokio::spawn(async move {
            let mut ticker = interval(Duration::from_secs(30));
            loop {
                ticker.tick().await;
                let mut sender = ws_sender_ping.lock().await;
                if sender
                    .send(tungstenite::Message::Ping(vec![].into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        // Status logger task
        let log_handle = tokio::spawn({
            let pending = pending.clone();
            let name = name.clone();
            async move {
                let mut ticker = interval(Duration::from_secs(60));
                loop {
                    ticker.tick().await;
                    let count = pending.lock().await.len();
                    log::info!("tun {} connected, pending={}", name, count);
                }
            }
        });

        // Main read loop
        while let Some(msg) = ws_read.next().await {
            match msg {
                Ok(tungstenite::Message::Binary(buf)) => {
                    // Try decompress raw DEFLATE, fallback to raw if not compressed
                    let buf = match deflate_decode(&buf) {
                        Ok(d) => d,
                        Err(_) => buf.to_vec(),
                    };
                    match deserialize_msgpack::<TunnelFrame>(&buf) {
                        Ok(TunnelFrame::Req(req)) => {
                            let pending = pending.clone();
                            let batch_tx = batch_tx.clone();
                            let http_client = http_client.clone();
                            let name = name.clone();
                            tokio::spawn(async move {
                                if let Err(e) =
                                    Self::handle_request(req, http_client, batch_tx, pending).await
                                {
                                    log::warn!("tun {} handle_request error: {}", name, e);
                                }
                            });
                        }
                        Ok(TunnelFrame::Res(_)) => {
                            log::warn!("unexpected response frame from ngx");
                        }
                        Ok(TunnelFrame::WsStart { rid, path }) => {
                            // WS relay: ngx requests that we connect to a backend.
                            // ngx expects 101 response via the msgpack channel (pending_ws map).
                            log::info!("WsStart rid={} path={}", rid, path);
                            let (resp_tx, _resp_rx) =
                                tokio::sync::oneshot::channel::<TunnelResponseFrame>();
                            tokio::spawn(async move {
                                Self::handle_ws_start(rid, path, resp_tx).await;
                            });
                        }
                        Ok(TunnelFrame::WsEnd { rid }) => {
                            // TODO: implement WS relay end
                            log::debug!("WsEnd rid={}", rid);
                        }
                        Err(e) => {
                            log::warn!("malformed frame from ngx: {}", e);
                        }
                    }
                }
                Ok(tungstenite::Message::Text(t)) => {
                    log::warn!(
                        "received text frame from ngx (expected binary msgpack): {}",
                        t
                    );
                }
                Ok(tungstenite::Message::Close(_)) => {
                    log::info!("tun {} received close from ngx", name);
                    break;
                }
                Ok(tungstenite::Message::Ping(_)) | Ok(tungstenite::Message::Pong(_)) => {
                    // Auto-handled by tungstenite
                }
                Err(e) => {
                    log::warn!("WS read error from ngx: {}", e);
                    break;
                }
                _ => continue,
            }
        }

        batch_handle.abort();
        ping_handle.abort();
        log_handle.abort();

        Ok(())
    }

    async fn handle_request(
        req: TunnelRequestFrame,
        http_client: Client,
        batch_tx: mpsc::Sender<TunnelResponseFrame>,
        pending: Arc<Mutex<HashMap<String, PendingRequest>>>,
    ) -> Result<()> {
        let rid = req.rid.clone();

        // Track pending request
        pending.lock().await.insert(rid.clone(), PendingRequest);

        // Proxy to backend
        let result = Self::proxy_request(req, &http_client).await;

        // Remove from pending
        pending.lock().await.remove(&rid);

        let resp_frame = match result {
            Ok(frame) => frame,
            Err(e) => {
                warn!("tun proxy error for rid {}: {}", rid, e);
                TunnelResponseFrame {
                    rid,
                    status: 502,
                    headers: vec![],
                    body: e.to_string().into_bytes(),
                }
            }
        };

        batch_tx
            .send(resp_frame)
            .await
            .map_err(|_| anyhow::anyhow!("batch channel closed"))?;
        Ok(())
    }

    async fn handle_ws_start(
        rid: String,
        path: String,
        resp_tx: tokio::sync::oneshot::Sender<TunnelResponseFrame>,
    ) {
        // Parse backend URL from path.
        // path format: "http://host:port/path", "https://...", "ws://...", "wss://..."
        // or just "/path" (implies http://localhost:8080).
        let backend_url = if path.starts_with("http://")
            || path.starts_with("https://")
            || path.starts_with("ws://")
            || path.starts_with("wss://")
        {
            path.clone()
        } else {
            format!("http://127.0.0.1:8080{}", path)
        };

        // Determine scheme: wss:// if original was https:// or wss://, otherwise ws://
        let ws_scheme = if backend_url.starts_with("https://") || backend_url.starts_with("wss://")
        {
            "wss"
        } else {
            "ws"
        };
        let backend_host = backend_url
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .trim_start_matches("ws://")
            .trim_start_matches("wss://");
        let ws_url = format!("{}://{}", ws_scheme, backend_host);
        log::info!("connecting to backend WS: {}", ws_url);

        match tokio_tungstenite::connect_async(&ws_url).await {
            Ok((_ws_backend, _)) => {
                log::info!("WS connected to backend: {}", ws_url);
                // Send the full URL (with scheme) so ngx can use correct ws/wss
                let resp = TunnelResponseFrame {
                    rid,
                    status: 101,
                    headers: vec![],
                    body: ws_url.into_bytes(),
                };
                let _ = resp_tx.send(resp);
            }
            Err(e) => {
                log::warn!("WS connect to backend {} failed: {}", ws_url, e);
            }
        }
    }

    async fn proxy_request(
        req: TunnelRequestFrame,
        http_client: &Client,
    ) -> Result<TunnelResponseFrame> {
        // req.path contains the full backend URL built by ngx:
        //   http://127.0.0.1:9020/some/path   (http backend)
        //   https://backend.internal/path      (https backend)
        //   file:///var/www/static/some/path   (static file backend)
        //
        // Fallback for legacy frames that only carry a bare path (e.g. "/"):
        // use Host header to build a local http:// URL so old behaviour is preserved.
        let path = req.path.as_str();
        let backend_url = if path.starts_with("http://")
            || path.starts_with("https://")
        {
            // HTTP/HTTPS: proxy via reqwest as usual
            path.to_string()
        } else if path.starts_with("file:///") {
            // Static file: handled specially below — return early
            return Self::serve_static_file(&req.rid, path).await;
        } else {
            // Bare path — legacy fallback: use Host header as the origin
            let host = req
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("Host"))
                .map(|(_, v)| v.as_str())
                .unwrap_or("127.0.0.1");
            format!("http://{}{}", host, path)
        };

        // Build HTTP/HTTPS request
        let method = http::Method::from_bytes(req.method.as_bytes())
            .map_err(|_| anyhow::anyhow!("invalid HTTP method"))?;
        let url = url::Url::parse(&backend_url)
            .map_err(|e| anyhow::anyhow!("invalid URL '{}': {}", backend_url, e))?;
        let mut request = reqwest::Request::new(method, url);

        for (k, v) in req.headers {
            let header_name = HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| anyhow::anyhow!("invalid header name '{}': {}", k, e))?;
            let header_value = HeaderValue::from_str(&v)
                .map_err(|e| anyhow::anyhow!("invalid header value for '{}': {}", k, e))?;
            request.headers_mut().insert(header_name, header_value);
        }

        if !req.body.is_empty() {
            *request.body_mut() = Some(reqwest::Body::from(req.body));
        }

        let resp = http_client.execute(request).await?;

        let status = resp.status().as_u16();
        let headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let body = resp.bytes().await?.to_vec();

        Ok(TunnelResponseFrame {
            rid: req.rid,
            status,
            headers,
            body,
        })
    }

    /// Serve a local static file for a `file:///` backend URL.
    ///
    /// The URL is the full path built by ngx, e.g.:
    ///   `file:///var/www/static/index.html`
    ///   `file:///home/alice/public/`
    ///
    /// Returns 200 with file contents, 404 if not found, 403 for directory
    /// requests without an index, or 500 on read error.
    async fn serve_static_file(rid: &str, file_url: &str) -> Result<TunnelResponseFrame> {
        // Strip "file://" prefix → absolute filesystem path
        let fs_path = file_url.strip_prefix("file://").unwrap_or(file_url);

        // Resolve index.html/index.htm for directory requests
        let resolved = if fs_path.ends_with('/') || std::path::Path::new(fs_path).is_dir() {
            let html = format!("{}/index.html", fs_path.trim_end_matches('/'));
            let htm = format!("{}/index.htm", fs_path.trim_end_matches('/'));
            if std::path::Path::new(&html).exists() {
                html
            } else if std::path::Path::new(&htm).exists() {
                htm
            } else {
                return Ok(TunnelResponseFrame {
                    rid: rid.to_string(),
                    status: 404,
                    headers: vec![("Content-Type".into(), "text/plain".into())],
                    body: b"Not Found".to_vec(),
                });
            }
        } else {
            fs_path.to_string()
        };

        match tokio::fs::read(&resolved).await {
            Ok(content) => {
                let mime = mime_guess::from_path(&resolved)
                    .first_or_octet_stream()
                    .to_string();
                Ok(TunnelResponseFrame {
                    rid: rid.to_string(),
                    status: 200,
                    headers: vec![
                        ("Content-Type".into(), mime),
                        ("Content-Length".into(), content.len().to_string()),
                    ],
                    body: content,
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TunnelResponseFrame {
                rid: rid.to_string(),
                status: 404,
                headers: vec![("Content-Type".into(), "text/plain".into())],
                body: b"Not Found".to_vec(),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Ok(TunnelResponseFrame {
                rid: rid.to_string(),
                status: 403,
                headers: vec![("Content-Type".into(), "text/plain".into())],
                body: b"Forbidden".to_vec(),
            }),
            Err(e) => Err(anyhow::anyhow!("file read error {}: {}", resolved, e)),
        }
    }
}
