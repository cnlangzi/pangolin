//! Built-in mock ngx server for unit testing.
//!
//! Simulates a minimal ngx server that:
//!   - Accepts WS connections at a given port
//!   - Validates token + name query params
//!   - Sends TunnelRequestFrame msgpack frames
//!   - Expects TunnelResponseFrame/TunnelFrame[] msgpack frames back
//!
//! Usage in tests:
//!   let mock = MockNgx::start().await;
//!   // spawn tun client connected to mock.addr()
//!   // mock.expect_request(...).await;
//!   mock.shutdown().await;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, Mutex};
use tokio_tungstenite::{accept_async, tungstenite};

use crate::frame::{deserialize_msgpack, serialize_frames, serialize_msgpack, TunnelFrame, TunnelRequestFrame, TunnelResponseFrame};

pub struct MockNgx {
    addr: String,
    requests: Arc<Mutex<Vec<TunnelRequestFrame>>>,
    _shutdown_tx: Arc<oneshot::Sender<()>>,
}

impl MockNgx {
    /// Start a mock ngx server on a random available port.
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_for_spawn = requests.clone();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let mut shutdown_rx_for_spawn = shutdown_rx;

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx_for_spawn => {
                        break;
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
            _shutdown_tx: Arc::new(shutdown_tx),
        }
    }

    /// Return the mock server's listen address.
    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// Stop the mock server.
    pub async fn shutdown(self) {
        // Dropping _shutdown_tx triggers oneshot close
    }

    /// Get all requests received by the mock.
    pub async fn get_requests(&self) -> Vec<TunnelRequestFrame> {
        self.requests.lock().await.clone()
    }

    /// Wait for and return the next request frame.
    /// Times out after `dur`.
    pub async fn next_request(&self, dur: Duration) -> Option<TunnelRequestFrame> {
        let requests = self.requests.clone();
        let deadline = tokio::time::Instant::now() + dur;
        loop {
            {
                let reqs = requests.lock().await;
                if !reqs.is_empty() {
                    let req = reqs[0].clone();
                    drop(reqs);
                    let mut r = requests.lock().await;
                    r.remove(0);
                    return Some(req);
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn handle_ws(stream: TcpStream, requests: Arc<Mutex<Vec<TunnelRequestFrame>>>) -> Result<()> {
        let ws = accept_async(stream).await?;
        let (mut ws_sender, mut ws_read) = ws.split();

        // Read one request
        if let Some(msg) = ws_read.next().await {
            match msg? {
                tungstenite::Message::Binary(buf) => {
                    match deserialize_msgpack::<TunnelFrame>(&buf) {
                        Ok(TunnelFrame::Req(req)) => {
                            requests.lock().await.push(req);
                        }
                        Ok(TunnelFrame::Res(_)) => {
                            // tun sent a response to our request
                        }
                        Err(e) => {
                            log::warn!("mock ngx malformed frame: {}", e);
                        }
                    }
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
    async fn test_mock_ngx_basic() {
        let mock = MockNgx::start().await;
        let addr = mock.addr();

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
        ws_sender.send(tungstenite::Message::Binary(buf)).await.unwrap();

        // Receive response
        ws_sender.send(tungstenite::Message::Binary(
            serialize_msgpack(&TunnelFrame::Res(TunnelResponseFrame {
                rid: "test-1".into(),
                status: 200,
                headers: vec![],
                body: b"ok".to_vec(),
            })).unwrap()
        )).await.unwrap();

        ws_sender.close().await.unwrap();

        // Wait for mock to process the request
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Check mock received the request
        let reqs = mock.get_requests().await;
        assert_eq!(reqs.len(), 1, "mock should have received 1 request");
        assert_eq!(reqs[0].rid, "test-1");
        assert_eq!(reqs[0].path, "/api/test");

        mock.shutdown().await;
    }
}
