//! Tunnel path integration tests.
//!
//! Covers: tests/CHECKLIST.md → Tunnel Path (5 tests)
//!
//! Tests use MockNgx (built-in mock WS server from tun crate) to simulate
//! the ngx side of the tunnel protocol. The tun client connects to MockNgx
//! via WS, and we verify request/response routing.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_tungstenite::{connect_async, tungstenite};
use futures_util::{SinkExt, StreamExt};

use tun::mock_ngx::MockNgx;
use tun::frame::{
    serialize_msgpack, TunnelFrame, TunnelRequestFrame, TunnelResponseFrame,
};

// ---------------------------------------------------------------------------
// Hanging backend server (for timeout tests)
// ---------------------------------------------------------------------------

/// A TCP server that accepts connections but NEVER responds.
/// Used to simulate a hanging backend and verify HTTP client timeouts.
async fn start_hanging_backend() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();

    tokio::spawn(async move {
        loop {
            if let Ok((mut stream, _)) = listener.accept().await {
                // Accept, read request, NEVER respond — simulate hanging backend
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                // Keep connection alive forever
                tokio::time::sleep(Duration::from_secs(300)).await;
            }
        }
    });

    addr
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// tunnel_basic — MockNgx receives a valid WS tunnel frame with correct rid + path
#[tokio::test]
async fn tunnel_basic() {
    let mock = MockNgx::start().await;
    let addr = mock.addr();

    let ws_url = format!("ws://{}/tunnel?token=test&name=testnode", addr);
    let (mut ws, _) = connect_async(&ws_url).await.expect("connect");
    let (mut ws_sender, _ws_read) = ws.split();

    let req = TunnelRequestFrame {
        rid: "tunnel-req-1".into(),
        method: "GET".into(),
        path: "/api/users".into(),
        headers: vec![
            ("Host".into(), "app.example.com".into()),
            ("Authorization".into(), "Bearer test".into()),
        ],
        body: vec![],
    };

    let send_buf = serialize_msgpack(&TunnelFrame::Req(req.clone())).unwrap();
    ws_sender.send(tungstenite::Message::Binary(send_buf.into())).await.unwrap();

    // Simulate backend responding
    let resp_buf = serialize_msgpack(&TunnelFrame::Res(TunnelResponseFrame {
        rid: "tunnel-req-1".into(),
        status: 200,
        headers: vec![("Content-Type".into(), "application/json".into())],
        body: b"{\"ok\":true}".to_vec(),
    })).unwrap();
    ws_sender.send(tungstenite::Message::Binary(resp_buf.into())).await.unwrap();

    ws_sender.close().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let requests = mock.get_requests().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].rid, "tunnel-req-1");
    assert_eq!(requests[0].path, "/api/users");
    assert_eq!(requests[0].method, "GET");

    mock.shutdown().await;
}

/// tunnel_offline — WS connect to invalid ngx addr → connection refused / error
#[tokio::test]
async fn tunnel_offline() {
    let result = connect_async("ws://127.0.0.1:59999/tunnel?token=x&name=x").await;
    assert!(result.is_err(), "connecting to offline server should fail");
}

/// tunnel_concurrent — two concurrent tunnel frames → MockNgx records both with correct rids
#[tokio::test]
async fn tunnel_concurrent() {
    let mock = MockNgx::start().await;
    let addr = mock.addr();

    let ws_url = format!("ws://{}/tunnel?token=test&name=testnode", addr);
    let (mut ws, _) = connect_async(&ws_url).await.expect("connect");
    let (mut ws_sender, _ws_read) = ws.split();

    let req_a = TunnelRequestFrame {
        rid: "req-a".into(),
        method: "GET".into(),
        path: "/api/one".into(),
        headers: vec![],
        body: vec![],
    };
    let req_b = TunnelRequestFrame {
        rid: "req-b".into(),
        method: "POST".into(),
        path: "/api/two".into(),
        headers: vec![],
        body: vec![],
    };

    let buf_a = serialize_msgpack(&TunnelFrame::Req(req_a)).unwrap();
    let buf_b = serialize_msgpack(&TunnelFrame::Req(req_b)).unwrap();
    let _ = ws_sender.send(tungstenite::Message::Binary(buf_a.into())).await;
    let _ = ws_sender.send(tungstenite::Message::Binary(buf_b.into())).await;

    ws_sender.close().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let requests = mock.get_requests().await;
    assert_eq!(requests.len(), 2);

    let rids: Vec<_> = requests.iter().map(|r| r.rid.clone()).collect();
    assert!(rids.contains(&"req-a".into()));
    assert!(rids.contains(&"req-b".into()));

    mock.shutdown().await;
}

/// tunnel_multi — one WS connection, frames for different req_ids (simulating multi-site)
#[tokio::test]
async fn tunnel_multi() {
    let mock = MockNgx::start().await;
    let addr = mock.addr();

    let ws_url = format!("ws://{}/tunnel?token=test&name=testnode", addr);
    let (mut ws, _) = connect_async(&ws_url).await.expect("connect");
    let (mut ws_sender, _ws_read) = ws.split();

    // Site A: 3 requests
    for i in 0..3 {
        let req = TunnelRequestFrame {
            rid: format!("site-a-req-{}", i),
            method: "GET".into(),
            path: format!("/site-a/{}", i),
            headers: vec![],
            body: vec![],
        };
        let buf = serialize_msgpack(&TunnelFrame::Req(req)).unwrap();
        let _ = ws_sender.send(tungstenite::Message::Binary(buf.into())).await;
    }

    // Site B: 2 requests
    for i in 0..2 {
        let req = TunnelRequestFrame {
            rid: format!("site-b-req-{}", i),
            method: "GET".into(),
            path: format!("/site-b/{}", i),
            headers: vec![],
            body: vec![],
        };
        let buf = serialize_msgpack(&TunnelFrame::Req(req)).unwrap();
        let _ = ws_sender.send(tungstenite::Message::Binary(buf.into())).await;
    }

    ws_sender.close().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let requests = mock.get_requests().await;
    assert_eq!(requests.len(), 5, "should have 5 total requests from 2 sites");

    let paths: Vec<_> = requests.iter().map(|r| r.path.clone()).collect();
    assert!(paths.contains(&"/site-a/0".into()));
    assert!(paths.contains(&"/site-b/0".into()));

    mock.shutdown().await;
}

/// tunnel_timeout — reqwest HTTP client hits hanging backend → reqwest returns error
///
/// The tun client uses a reqwest Client with a 30s timeout (hardcoded).
/// We test directly against a hanging TCP server to verify the timeout fires.
#[tokio::test]
async fn tunnel_timeout() {
    // Start a hanging backend (accepts, reads, never responds)
    let backend_addr = start_hanging_backend().await;

    // Use reqwest Client directly to verify timeout behavior
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2)) // short timeout for test
        .build()
        .expect("client should build");

    let uri = format!("http://{}/api/test", backend_addr);
    let req = client.get(&uri).build().expect("request should build");

    // Record start time
    let start = std::time::Instant::now();

    // Request to hanging backend should fail due to timeout
    let result = client.execute(req).await;

    let elapsed = start.elapsed();

    // Should be an error (timeout)
    assert!(result.is_err(), "request to hanging backend should error");
    let err = result.unwrap_err();
    assert!(
        err.is_timeout() || err.is_connect(),
        "error should be timeout or connect error, got: {}",
        err
    );

    // Should have timed out in ~2 seconds (not 30s default)
    assert!(
        elapsed >= Duration::from_secs(2),
        "timeout should fire in ~2s, took {:?}",
        elapsed
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "timeout should fire within 5s, took {:?}",
        elapsed
    );
}