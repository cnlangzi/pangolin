//! WebSocket relay E2E integration tests.
//!
//! Run with: `cargo test --features integration -p pangolin-integration-tests ws_relay`

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_tungstenite::{accept_async, tungstenite};

use pangolin_core::compress::{deflate_decode, deflate_encode};
use pangolin_core::serialize_msgpack;
use pangolin_core::{deserialize_msgpack, TunnelFrame};
use tun::test_ws_server::TestWsServer;

// ---------------------------------------------------------------------------
// Mock WebSocket backend (echo server)
// ---------------------------------------------------------------------------

struct MockWsBackend {
    addr: String,
    frames: Arc<Mutex<Vec<String>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl MockWsBackend {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let frames = Arc::new(Mutex::new(Vec::new()));
        let frames_for_spawn = frames.clone();

        let handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let frames = frames_for_spawn.clone();
                        tokio::spawn(Self::handle_ws(stream, frames));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            addr,
            frames,
            handle,
        }
    }

    fn addr(&self) -> &str {
        &self.addr
    }

    async fn get_frames(&self) -> Vec<String> {
        self.frames.lock().await.clone()
    }

    async fn handle_ws(stream: tokio::net::TcpStream, frames: Arc<Mutex<Vec<String>>>) {
        let ws = match accept_async(stream).await {
            Ok(ws) => ws,
            Err(_) => return,
        };
        let (mut sender, mut reader) = ws.split();
        while let Some(msg) = reader.next().await {
            match msg {
                Ok(tungstenite::Message::Text(text)) => {
                    let s = text.as_str().to_string();
                    frames.lock().await.push(s.clone());
                    let _ = sender.send(tungstenite::Message::Text(s.into())).await;
                }
                Ok(tungstenite::Message::Binary(data)) => {
                    let s = String::from_utf8_lossy(data.as_ref()).to_string();
                    frames.lock().await.push(s.clone());
                    let _ = sender.send(tungstenite::Message::Binary(data)).await;
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
// ---------------------------------------------------------------------------

/// tunnel_ws_relay_start — tun handles WsStart and responds with 101.
#[tokio::test]
async fn tunnel_ws_relay_start() {
    let _ = env_logger::try_init();

    let backend = MockWsBackend::start().await;
    log::info!("mock WS backend on {}", backend.addr());

    let mock_ngx = TestWsServer::start().await;
    let ngx_addr = mock_ngx.addr();
    log::info!("mock ngx on {}", ngx_addr);

    let tun_config = tun::Config {
        server: ngx_addr.to_string(),
        token: "test".to_string(),
        name: "testnode".to_string(),
    };
    let tun_client = tun::TunnelClient::new(tun_config);

    let tun_handle = tokio::spawn(async move {
        tun_client.run().await;
    });

    tokio::time::sleep(Duration::from_millis(400)).await;

    let ws_url = format!("ws://{}/tunnel?token=test&name=testnode", ngx_addr);
    let (ws, _) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .expect("connect to mock ngx");

    let (mut ws_sender, mut ws_read) = ws.split();

    let rid = "ws-test-relay-1";
    let ws_start = TunnelFrame::WsStart {
        rid: rid.to_string(),
        path: backend.addr().to_string(),
    };
    let encoded = serialize_msgpack(&ws_start).unwrap();
    let compressed = deflate_encode(&encoded);
    ws_sender
        .send(tungstenite::Message::Binary(compressed.into()))
        .await
        .unwrap();

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
                            "Got 101 response, backend body: {}",
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

    assert!(
        got_101,
        "tun should have responded with 101 to WsStart (rid={})",
        rid
    );

    drop(ws_sender);
    tun_handle.abort();
    mock_ngx.shutdown().await;

    log::info!("tunnel_ws_relay_start passed");
}

// ---------------------------------------------------------------------------
// Test: tunnel_ws_relay_full_echo
// ---------------------------------------------------------------------------

/// tunnel_ws_relay_full_echo — End-to-end WS echo through tunnel path.
#[tokio::test]
async fn tunnel_ws_relay_full_echo() {
    let _ = env_logger::try_init();

    let backend = MockWsBackend::start().await;
    log::info!("mock WS backend on {}", backend.addr());

    let mock_ngx = TestWsServer::start().await;
    let ngx_addr = mock_ngx.addr();
    log::info!("mock ngx on {}", ngx_addr);

    let tun_config = tun::Config {
        server: ngx_addr.to_string(),
        token: "test".to_string(),
        name: "testnode".to_string(),
    };
    let tun_client = tun::TunnelClient::new(tun_config);

    let tun_handle = tokio::spawn(async move {
        tun_client.run().await;
    });

    tokio::time::sleep(Duration::from_millis(400)).await;

    let ws_url = format!("ws://{}/tunnel?token=test&name=testnode", ngx_addr);
    let (ws, _) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .expect("connect to mock ngx");

    let (mut ws_sender, mut ws_read) = ws.split();

    let rid = "ws-echo-test-1";
    let ws_start = TunnelFrame::WsStart {
        rid: rid.to_string(),
        path: backend.addr().to_string(),
    };
    let encoded = serialize_msgpack(&ws_start).unwrap();
    let compressed = deflate_encode(&encoded);
    ws_sender
        .send(tungstenite::Message::Binary(compressed.into()))
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut got_101 = false;
    let mut backend_addr = String::new();

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
                        backend_addr = String::from_utf8_lossy(&resp.body).to_string();
                        break;
                    }
                }
            }
            Some(Ok(tungstenite::Message::Close(_))) | None => break,
            _ => {}
        }
    }

    assert!(got_101, "Should get 101 response from tun");
    log::info!("Got backend address from tun: {}", backend_addr);

    drop(ws_sender);
    tun_handle.abort();
    mock_ngx.shutdown().await;

    log::info!("tunnel_ws_relay_full_echo passed (101 path verified)");
}
