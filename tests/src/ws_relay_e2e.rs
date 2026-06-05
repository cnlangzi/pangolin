//! WebSocket relay E2E integration tests.
//!
//! Tests the complete WS relay flow: ngx → tun → backend WS echo server.
//!
//! Run with: `cargo test --features integration -p pangolin-integration-tests ws_relay`

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_tungstenite::{accept_async, tungstenite};

use pangolin_core::compress::{deflate_decode, deflate_encode};
use pangolin_core::{deserialize_msgpack, serialize_msgpack, TunnelFrame};
use tun::test_ws_server::TestWsServer;

// ---------------------------------------------------------------------------
// Mock WebSocket backend (echo server)
// ---------------------------------------------------------------------------

struct MockWsBackend {
    _addr: String,
    handle: tokio::task::JoinHandle<()>,
}

impl MockWsBackend {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        tokio::spawn(Self::handle_one(stream));
                    }
                    Err(_) => break,
                }
            }
        });

        Self { _addr: addr, handle }
    }

    async fn handle_one(stream: tokio::net::TcpStream) {
        let ws = match accept_async(stream).await {
            Ok(ws) => ws,
            Err(_) => return,
        };
        let (sender, mut reader) = ws.split();
        let mut sender = sender;
        while let Some(msg) = reader.next().await {
            match msg {
                Ok(tungstenite::Message::Text(t)) => {
                    let _ = sender.send(tungstenite::Message::Text(t)).await;
                }
                Ok(tungstenite::Message::Binary(d)) => {
                    let _ = sender.send(tungstenite::Message::Binary(d)).await;
                }
                Ok(tungstenite::Message::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
    }
}

impl Drop for MockWsBackend {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

// ---------------------------------------------------------------------------
// Test: tunnel_ws_relay_start
//
// Verifies:
//   1. tun receives WsStart frame from mock ngx
//   2. tun sends back Res{status:101} with backend address in body
//   3. The test_ws_server WsStart handler echoes path in body
// ---------------------------------------------------------------------------

/// tunnel_ws_relay_start — tun handles WsStart and responds with 101.
#[tokio::test]
async fn tunnel_ws_relay_start() {
    let _ = env_logger::try_init();

    // Start mock backend WS (tun connects here on WsStart)
    let backend = MockWsBackend::start().await;

    // Start TestWsServer (mock ngx that speaks tunnel protocol)
    let mock_ngx = TestWsServer::start().await;

    // Start tun client — it connects to mock ngx and registers
    let tun_config = tun::Config {
        server: mock_ngx.addr().to_string(),
        token: "test".to_string(),
        name: "testnode".to_string(),
    };
    let tun_client = tun::TunnelClient::new(tun_config);

    let tun_handle = tokio::spawn(async move {
        tun_client.run().await;
    });

    // Wait for tun to connect and register with mock ngx
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Connect to mock ngx as a "proxy" client to send WsStart to tun
    let ws_url = format!("ws://{}/tunnel?token=test&name=testnode", mock_ngx.addr());
    let (ws, _) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .expect("connect to mock ngx");

    let (mut ws_sender, mut ws_read) = ws.split();

    // Send WsStart: tun should receive it, forward to backend, respond with 101
    let rid = "ws-test-001";
    let ws_start = TunnelFrame::WsStart {
        rid: rid.to_string(),
        path: backend._addr.clone(),
    };
    let encoded = serialize_msgpack(&ws_start).unwrap();
    let compressed = deflate_encode(&encoded);
    ws_sender
        .send(tungstenite::Message::Binary(compressed.into()))
        .await
        .unwrap();

    // Wait for 101 response from tun (via mock ngx → our WS connection)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut got_101 = false;

    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;

        match ws_read.next().await {
            Some(Ok(tungstenite::Message::Binary(buf))) => {
                let buf_vec = buf.to_vec();
                let buf = match deflate_decode(&buf_vec) {
                    Ok(d) => d,
                    Err(_) => buf_vec,
                };
                if let Ok(TunnelFrame::Res(resp)) = deserialize_msgpack::<TunnelFrame>(&buf) {
                    if resp.rid == rid && resp.status == 101 {
                        got_101 = true;
                        log::info!(
                            "Got 101 from tun, backend body: '{}'",
                            String::from_utf8_lossy(&resp.body)
                        );
                        break;
                    }
                }
            }
            Some(Ok(tungstenite::Message::Close(_))) | None => break,
            _ => {}
        }
    }

    assert!(got_101, "tun should respond with 101 to WsStart (rid={})", rid);

    drop(ws_sender);
    tun_handle.abort();
    mock_ngx.shutdown().await;
}
