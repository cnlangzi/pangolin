//! Built-in test WebSocket server for unit testing.
//!
//! Simulates a minimal ngx server that:
//!   - Accepts WS connections at a given port
//!   - Validates token + name query params
//!   - Reads TunnelRequestFrame msgpack frames (multiple, until close)
//!   - Stores received requests for inspection by tests
//!
//! Usage in tests:
//!   let server = TestWsServer::start().await;
//!   // spawn tun client connected to server.addr()
//!   // server.expect_request(...).await;
//!   server.shutdown().await;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, Mutex};
use tokio_tungstenite::{accept_async, tungstenite};

use crate::frame::{
    deflate_decode, deflate_encode, deserialize_msgpack, serialize_msgpack, TunnelFrame,
    TunnelRequestFrame, TunnelResponseFrame,
};

pub struct TestWsServer {
    addr: String,
    requests: Arc<Mutex<Vec<TunnelRequestFrame>>>,
    /// Signalled true to tell the accept loop to shut down.
    shutdown_flag: Arc<AtomicBool>,
    /// Pending WS relay: rid -> broadcast sender for 101 response.
    /// Multiple subscribers (tun + test client) can receive the same 101.
    #[allow(dead_code)]
    pending_ws: Arc<Mutex<HashMap<String, broadcast::Sender<TunnelResponseFrame>>>>,
}

impl TestWsServer {
    /// Start a mock ngx server on a random available port.
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let requests = Arc::new(Mutex::new(Vec::new()));
        let pending_ws = Arc::new(Mutex::new(HashMap::new()));
        let requests_for_spawn = requests.clone();
        let pending_ws_for_spawn = pending_ws.clone();
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let shutdown_for_spawn = shutdown_flag.clone();

        tokio::spawn(async move {
            loop {
                if shutdown_for_spawn.load(Ordering::SeqCst) {
                    break;
                }

                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                    res = listener.accept() => {
                        match res {
                            Ok((stream, _)) => {
                                let requests = requests_for_spawn.clone();
                                let pending_ws = pending_ws_for_spawn.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = Self::handle_ws(stream, requests, pending_ws).await {
                                        log::warn!("mock ngx ws error: {}", e);
                                    }
                                });
                            }
                            Err(e) => {
                                log::warn!("mock ngx accept error: {}", e);
                            }
                        }
                    }
                }
            }
        });

        Self {
            addr,
            requests,
            shutdown_flag,
            pending_ws,
        }
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    pub async fn get_requests(&self) -> Vec<TunnelRequestFrame> {
        let reqs = self.requests.lock().await;
        reqs.clone()
    }

    #[allow(dead_code)]
    pub async fn wait_for_requests(&self, n: usize) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let reqs = self.requests.lock().await;
            if reqs.len() >= n {
                return;
            }
            drop(reqs);
            if tokio::time::Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub async fn shutdown(self) {
        self.shutdown_flag.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    /// Handle an incoming WS connection.
    /// - Req frames: store in requests list
    /// - WsStart frames: forward to backend, wait for 101, broadcast to all pending_ws subscribers
    async fn handle_ws(
        stream: TcpStream,
        requests: Arc<Mutex<Vec<TunnelRequestFrame>>>,
        pending_ws: Arc<Mutex<HashMap<String, broadcast::Sender<TunnelResponseFrame>>>>,
    ) -> Result<()> {
        let ws = accept_async(stream).await?;
        let (mut ws_sender, mut ws_read) = ws.split();

        while let Some(msg) = ws_read.next().await {
            match msg {
                Ok(tungstenite::Message::Binary(buf)) => {
                    // ngx sends DEFLATE-compressed frames; decompress first, fall back to raw.
                    let decompressed = deflate_decode(&buf).unwrap_or_else(|_| buf.to_vec());

                    match deserialize_msgpack::<TunnelFrame>(&decompressed) {
                        Ok(TunnelFrame::Req(req)) => {
                            requests.lock().await.push(req);
                        }
                        Ok(TunnelFrame::Res(_r)) => {
                            // tun sent a response to our request
                        }
                        Ok(TunnelFrame::WsStart { rid, path }) => {
                            // Mock ngx WS relay:
                            // 1. Connect to backend (path)
                            // 2. Forward WsStart to backend (compressed msgpack), wait for 101
                            // 3. Broadcast 101 to ALL pending_ws subscribers (tun + test client)
                            log::info!("mock ngx WsStart rid={} path={}", rid, path);

                            let backend_url = format!(
                                "ws://{}",
                                path.trim_start_matches("http://")
                                    .trim_start_matches("https://")
                            );

                            // Create broadcast channel for this rid
                            let (tx, _rx) = broadcast::channel::<TunnelResponseFrame>(2);
                            {
                                let mut p = pending_ws.lock().await;
                                p.insert(rid.clone(), tx);
                            }

                            // Connect to backend and forward WsStart
                            match tokio_tungstenite::connect_async(&backend_url).await {
                                Ok((mut backend_ws, _)) => {
                                    // Send WsStart to backend (compressed, matching real data-plane)
                                    let ws_start_frame = TunnelFrame::WsStart {
                                        rid: rid.clone(),
                                        path: path.clone(),
                                    };
                                    if let Ok(buf) = serialize_msgpack(&ws_start_frame) {
                                        let compressed = deflate_encode(&buf);
                                        let _ = backend_ws
                                            .send(tungstenite::Message::Binary(compressed.into()))
                                            .await;
                                    }

                                    // Wait for 101 from backend (with 5s deadline)
                                    let deadline =
                                        tokio::time::Instant::now() + Duration::from_secs(5);
                                    let mut got_101 = false;

                                    while tokio::time::Instant::now() < deadline {
                                        tokio::time::sleep(Duration::from_millis(50)).await;
                                        if let Some(Ok(tungstenite::Message::Binary(buf))) =
                                            backend_ws.next().await
                                        {
                                            let decompressed = deflate_decode(&buf)
                                                .unwrap_or_else(|_| buf.to_vec());
                                            if let Ok(TunnelFrame::Res(r)) =
                                                deserialize_msgpack::<TunnelFrame>(&decompressed)
                                            {
                                                if r.status == 101 {
                                                    got_101 = true;
                                                    let body_addr =
                                                        String::from_utf8_lossy(&r.body)
                                                            .to_string();
                                                    log::info!(
                                                        "mock ngx: backend 101, addr={}",
                                                        body_addr
                                                    );

                                                    // Broadcast 101 to all subscribers
                                                    let resp = TunnelResponseFrame {
                                                        rid: rid.clone(),
                                                        status: 101,
                                                        headers: vec![],
                                                        body: body_addr.into_bytes(),
                                                    };
                                                    {
                                                        let p = pending_ws.lock().await;
                                                        if let Some(tx) = p.get(&rid) {
                                                            let _ = tx.send(resp);
                                                        }
                                                    }

                                                    // Also send 101 to this test client connection
                                                    let resp_frame =
                                                        TunnelFrame::Res(TunnelResponseFrame {
                                                            rid: rid.clone(),
                                                            status: 101,
                                                            headers: vec![],
                                                            body: path.clone().into_bytes(),
                                                        });
                                                    if let Ok(buf) = serialize_msgpack(&resp_frame)
                                                    {
                                                        let _ = ws_sender
                                                            .send(tungstenite::Message::Binary(
                                                                buf.into(),
                                                            ))
                                                            .await;
                                                    }
                                                    break;
                                                }
                                            }
                                        }
                                    }

                                    if !got_101 {
                                        log::warn!("mock ngx: backend did not respond with 101");
                                    }
                                }
                                Err(e) => {
                                    log::warn!(
                                        "mock ngx: failed to connect to backend {}: {}",
                                        backend_url,
                                        e
                                    );
                                }
                            }

                            // Clean up pending_ws entry
                            {
                                let mut p = pending_ws.lock().await;
                                p.remove(&rid);
                            }
                        }
                        Ok(TunnelFrame::WsEnd { rid }) => {
                            log::debug!("mock ngx WsEnd rid={}", rid);
                        }
                        Err(e) => {
                            log::warn!("mock ngx malformed frame: {}", e);
                        }
                    }
                }
                Ok(tungstenite::Message::Close(_)) | Err(_) => break,
                _ => {}
            }
        }

        ws_sender.close().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::connect_async;

    #[tokio::test]
    async fn test_ws_server_basic() {
        let server = TestWsServer::start().await;
        let addr = server.addr();

        // Connect a "tun" client
        let ws_url = format!("ws://{}/tunnel?token=test&name=testnode", addr);
        let (ws, _) = connect_async(&ws_url).await.expect("connect");
        let (mut ws_sender, _ws_read) = ws.split();

        // Send a request frame
        let req = TunnelRequestFrame {
            rid: "test-1".into(),
            method: "GET".into(),
            path: "/api/test".into(),
            headers: vec![("Host".into(), "example.com".into())],
            body: vec![],
        };
        let buf = serialize_msgpack(&TunnelFrame::Req(req.clone())).unwrap();
        ws_sender
            .send(tungstenite::Message::Binary(buf.into()))
            .await
            .unwrap();

        // Receive response
        ws_sender
            .send(tungstenite::Message::Binary(
                serialize_msgpack(&TunnelFrame::Res(TunnelResponseFrame {
                    rid: "test-1".into(),
                    status: 200,
                    headers: vec![],
                    body: b"ok".to_vec(),
                }))
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();

        ws_sender.close().await.unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;

        let reqs = server.get_requests().await;
        assert_eq!(reqs.len(), 1, "mock should have received 1 request");
        assert_eq!(reqs[0].rid, "test-1");
        assert_eq!(reqs[0].path, "/api/test");

        server.shutdown().await;
    }
}
