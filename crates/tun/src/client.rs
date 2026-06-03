//! Pangolin tunnel node (tun) — WebSocket client implementation.

use std::time::Duration;
use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use log::{error, info, warn};
use reqwest::{Client, header::{HeaderName, HeaderValue}};
use tokio::sync::{oneshot, Mutex};
use tokio::time::{interval, sleep, Instant};
use tokio_tungstenite::{connect_async, tungstenite};

use crate::frame::{deserialize_msgpack, serialize_msgpack, TunnelFrame, TunnelRequestFrame, TunnelResponseFrame};

pub struct Config {
    pub server: String,
    pub token: String,
    pub name: String,
}

struct PendingRequest {
    sender: oneshot::Sender<TunnelResponseFrame>,
    created_at: Instant,
}

pub struct TunnelClient {
    config: Config,
    http_client: Client,
    pending: Arc<std::sync::Mutex<HashMap<String, PendingRequest>>>,
}

impl TunnelClient {
    pub fn new(config: Config) -> Self {
        let http_client = Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest client should build");

        Self {
            config,
            http_client,
            pending: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    pub async fn run(&self) {
        log::info!("tun {} starting, target {}",
            self.config.name, self.config.server);

        let mut reconnect_delay_secs: u64 = 1;
        let max_delay_secs: u64 = 30;

        loop {
            match self.connect_and_handle().await {
                Ok(()) => {
                    log::info!("tun {} disconnected normally", self.config.name);
                    break;
                }
                Err(e) => {
                    log::error!("tun {} connection error: {}, reconnecting in {}s",
                        self.config.name, e, reconnect_delay_secs);
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

        log::info!("connecting to {}", ws_url);
        let (ws_stream, _) = connect_async(&ws_url).await?;
        log::info!("tun {} connected to ngx", self.config.name);

        self.handle_stream(ws_stream).await
    }

    async fn handle_stream(
        &self,
        ws: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    ) -> Result<()> {
        let (ws_sender, mut ws_read) = ws.split();
        let pending = self.pending.clone();
        let name = self.config.name.clone();
        let http_client = self.http_client.clone();

        // Arc-wrapped sender so multiple concurrent request handlers can send
        let ws_sender = Arc::new(Mutex::new(ws_sender));

        // Keepalive ping task
        let ws_sender_ping = ws_sender.clone();
        let ping_handle = tokio::spawn(async move {
            let mut ticker = interval(Duration::from_secs(30));
            loop {
                ticker.tick().await;
                let mut sender = ws_sender_ping.lock().await;
                if sender.send(tungstenite::Message::Ping(vec![].into())).await.is_err() {
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
                    let count = pending.lock().unwrap().len();
                    log::info!("tun {} connected, pending={}", name, count);
                }
            }
        });

        // Main read loop
        while let Some(msg) = ws_read.next().await {
            match msg {
                Ok(tungstenite::Message::Binary(buf)) => {
                    match deserialize_msgpack::<TunnelFrame>(&buf) {
                        Ok(TunnelFrame::Req(req)) => {
                            let ws_sender = ws_sender.clone();
                            let http_client = http_client.clone();
                            let name = name.clone();
                            tokio::spawn(async move {
                                if let Err(e) = Self::handle_request(req, http_client, ws_sender).await {
                                    log::warn!("tun {} handle_request error: {}", name, e);
                                }
                            });
                        }
                        Ok(TunnelFrame::Res(_)) => {
                            log::warn!("unexpected response frame from ngx");
                        }
                        Err(e) => {
                            log::warn!("malformed frame from ngx: {}", e);
                        }
                    }
                }
                Ok(tungstenite::Message::Text(t)) => {
                    log::warn!("received text frame from ngx (expected binary msgpack): {}", t);
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

        ping_handle.abort();
        log_handle.abort();

        Ok(())
    }

    async fn handle_request(
        req: TunnelRequestFrame,
        http_client: Client,
        ws_sender: Arc<Mutex<futures_util::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
            tungstenite::Message,
        >>>,
    ) -> Result<()> {
        // Determine backend URL from Host header + path
        let host = req.headers.iter()
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

        let resp = http_client.execute(request).await?;

        let status = resp.status().as_u16();
        let headers: Vec<(String, String)> = resp.headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let body = resp.bytes().await?.to_vec();

        let response_frame = TunnelResponseFrame {
            rid: req.rid,
            status,
            headers,
            body,
        };

        let buf = serialize_msgpack(&TunnelFrame::Res(response_frame))?;
        let mut sender = ws_sender.lock().await;
        sender.send(tungstenite::Message::Binary(buf)).await?;

        Ok(())
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
    if !config.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        anyhow::bail!("name '{}' must match ^[a-zA-Z0-9_-]+$", config.name);
    }
    if config.name.len() > 32 {
        anyhow::bail!("name must be at most 32 characters");
    }
    if config.server.is_empty() {
        anyhow::bail!("server must not be empty");
    }
    Ok(())
}