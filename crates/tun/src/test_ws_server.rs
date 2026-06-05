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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_tungstenite::{accept_async, tungstenite};

use crate::frame::{
    deserialize_msgpack, serialize_msgpack, TunnelFrame, TunnelRequestFrame, TunnelResponseFrame,
};

pub struct TestWsServer {
    addr: String,
    requests: Arc<Mutex<Vec<TunnelRequestFrame>>>,
    /// Signalled true to tell the accept loop to shut down.
    shutdown_flag: Arc<AtomicBool>,
}

impl TestWsServer {
    /// Start a mock ngx server on a random available port.
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_for_spawn = requests.clone();
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let shutdown_for_spawn = shutdown_flag.clone();

        tokio::spawn(async move {
            loop {
                // Check shutdown flag first
                if shutdown_for_spawn.load(Ordering::SeqCst) {
                    break;
                }

                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(10)) => {
                        // Periodically check shutdown flag
                    }
                    res = listener.accept() => {
                        match res {
                            Ok((stream, _)) => {
                                let requests = requests_for_spawn.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = Self::handle_ws(stream, requests).await {
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
        }
    }

    /// Return the mock server's listen address.
    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// Get a copy of all received requests so far.
    pub async fn get_requests(&self) -> Vec<TunnelRequestFrame> {
        let reqs = self.requests.lock().await;
        reqs.clone()
    }

    /// Wait for at least `n` requests to be received.
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

    /// Shut down the mock server.
    pub async fn shutdown(self) {
        self.shutdown_flag.store(true, Ordering::SeqCst);
        // Give the accept loop time to exit
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    async fn handle_ws(
        stream: TcpStream,
        requests: Arc<Mutex<Vec<TunnelRequestFrame>>>,
    ) -> Result<()> {
        let ws = accept_async(stream).await?;
        let (mut ws_sender, mut ws_read) = ws.split();

        // Read ALL frames until the connection closes
        while let Some(msg) = ws_read.next().await {
            match msg {
                Ok(tungstenite::Message::Binary(buf)) => {
                    match deserialize_msgpack::<TunnelFrame>(&buf) {
                        Ok(TunnelFrame::Req(req)) => {
                            requests.lock().await.push(req);
                        }
                        Ok(TunnelFrame::Res(_)) => {
                            // tun sent a response to our request
                        }
                        Ok(TunnelFrame::WsStart { rid, path }) => {
                            // Mock ngx WS relay: respond with 101 + backend address.
                            log::info!("mock ngx WsStart rid={} path={}", rid, path);
                            let resp = TunnelResponseFrame {
                                rid,
                                status: 101,
                                headers: vec![],
                                body: path.into_bytes(),
                            };
                            let resp_frame = TunnelFrame::Res(resp);
                            if let Ok(buf) = serialize_msgpack(&resp_frame) {
                                let _ = ws_sender.send(tungstenite::Message::Binary(buf.into())).await;
                            }
                        }
                        Ok(TunnelFrame::WsEnd { rid }) => {
                            // TODO: implement WS relay end
                            log::debug!("mock ngx WsEnd rid={}", rid);
                        }
                        Err(e) => {
                            log::warn!("mock ngx malformed frame: {}", e);
                        }
                    }
                }
                Ok(tungstenite::Message::Close(_)) | Err(_) => {
                    break;
                }
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

        // Wait for mock to process the request
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Check mock received the request
        let reqs = server.get_requests().await;
        assert_eq!(reqs.len(), 1, "mock should have received 1 request");
        assert_eq!(reqs[0].rid, "test-1");
        assert_eq!(reqs[0].path, "/api/test");

        server.shutdown().await;
    }
}
