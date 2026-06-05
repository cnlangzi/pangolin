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

use crate::frame::{
    deflate_decode, deflate_encode, deserialize_msgpack, serialize_frames, TunnelFrame,
    TunnelRequestFrame, TunnelResponseFrame,
};

pub struct Config {
    pub server: String,
    pub token: String,
    pub name: String,
}

struct PendingRequest;

pub struct TunnelClient {
    config: Config,
    http_client: Client,
    pending: Arc<Mutex<HashMap<String, PendingRequest>>>,
}

impl TunnelClient {
    pub fn new(config: Config) -> Self {
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

        let mut reconnect_delay_secs: u64 = 1;
        let max_delay_secs: u64 = 30;

        loop {
            match self.connect_and_handle().await {
                Ok(()) => {
                    log::info!("tun {} disconnected normally", self.config.name);
                    break;
                }
                Err(e) => {
                    log::error!(
                        "tun {} connection error: {}, reconnecting in {}s",
                        self.config.name,
                        e,
                        reconnect_delay_secs
                    );
                    sleep(Duration::from_secs(reconnect_delay_secs)).await;
                    reconnect_delay_secs = (reconnect_delay_secs * 2).min(max_delay_secs);
                }
            }
        }
    }

    async fn connect_and_handle(&self) -> Result<()> {
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
        let (ws_stream, _) = connect_async(&ws_url).await?;
        log::info!("tun {} connected to ngx", self.config.name);

        self.handle_stream(ws_stream).await
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
                            let frames: Vec<TunnelFrame> = batch.drain(..).map(TunnelFrame::Res).collect();
                            if !frames.is_empty() {
                                if let Ok(buf) = serialize_frames(&frames) {
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
                        let frames: Vec<TunnelFrame> = batch.drain(..).map(TunnelFrame::Res).collect();
                        if !frames.is_empty() {
                            if let Ok(buf) = serialize_frames(&frames) {
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
                let frames: Vec<TunnelFrame> = batch.drain(..).map(TunnelFrame::Res).collect();
                if let Ok(buf) = serialize_frames(&frames) {
                    let compressed = deflate_encode(&buf);
                    let mut sender = ws_sender_batch.lock().await;
                    let _ = sender
                        .send(tungstenite::Message::Binary(compressed.into()))
                        .await;
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
        // path format: "http://host:port/path" or just "/path" (implies localhost).
        let backend_url = if path.starts_with("http://") || path.starts_with("https://") {
            path.clone()
        } else {
            format!("http://127.0.0.1:8080{}", path)
        };

        let backend_host = backend_url
            .trim_start_matches("http://")
            .trim_start_matches("https://");
        let ws_url = format!("ws://{}", backend_host);
        log::info!("connecting to backend WS: {}", ws_url);

        match tokio_tungstenite::connect_async(&ws_url).await {
            Ok((_ws_backend, _)) => {
                log::info!("WS connected to backend: {}", ws_url);
                let addr = backend_host.to_string();
                let resp = TunnelResponseFrame {
                    rid,
                    status: 101,
                    headers: vec![],
                    body: addr.into_bytes(),
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
        // Determine backend URL from Host header + path
        let host = req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("Host"))
            .map(|(_, v)| v.as_str())
            .unwrap_or("127.0.0.1");

        let path = req.path.as_str();
        let backend_url = if path.starts_with("http://") || path.starts_with("https://") {
            path.to_string()
        } else {
            format!("http://{}{}", host, path)
        };

        // Build request
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

        // Add request body if present
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
}

/// Validate CLI arguments.
pub fn validate_config(config: &Config) -> Result<()> {
    if config.token.is_empty() {
        anyhow::bail!("token must not be empty");
    }
    if config.name.is_empty() {
        anyhow::bail!("name must not be empty");
    }
    // Must match ^[a-z0-9_-]+$ (1~32 chars, lowercase only, not purely numeric)
    if config.name.len() > 32 {
        anyhow::bail!("name must be at most 32 characters");
    }
    if !config
        .name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        anyhow::bail!(
            "name '{}' must match ^[a-z0-9_-]+$ (lowercase letters, digits, dash, underscore only)",
            config.name
        );
    }
    if config.name.chars().all(|c| c.is_ascii_digit()) {
        anyhow::bail!("name '{}' cannot be purely numeric", config.name);
    }
    if config.server.is_empty() {
        anyhow::bail!("server must not be empty");
    }
    Ok(())
}
