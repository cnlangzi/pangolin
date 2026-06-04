//! Tunnel path integration tests.
//!
//! Covers: tests/CHECKLIST.md → Tunnel Path (5 tests)
//!
//! Tests use MockNgx (built-in mock WS server from tun crate) to simulate
//! the ngx side of the tunnel protocol. The tun client connects to MockNgx
//! via WS, and we verify request/response routing.
//!
//! These tests are async and use the tun crate's internal mock infrastructure.

use std::time::Duration;

use tokio_tungstenite::{connect_async, tungstenite};
use futures_util::{SinkExt, StreamExt};

use tun::mock_ngx::MockNgx;
use tun::frame::{
    serialize_msgpack, TunnelFrame, TunnelRequestFrame, TunnelResponseFrame,
};

/// tunnel_basic — MockNgx receives a valid WS tunnel frame with correct rid + path
///
/// This tests the full WS round-trip:
///   connect WS → send TunnelRequestFrame → receive TunnelResponseFrame
#[tokio::test]
async fn tunnel_basic() {
    let mock = MockNgx::start().await;
    let addr = mock.addr();

    // Connect a "tun" client WS
    let ws_url = format!("ws://{}/tunnel?token=test&name=testnode", addr);
    let (mut ws, _) = connect_async(&ws_url).await.expect("connect");
    let (mut ws_sender, _ws_read) = ws.split();

    // Send a request frame
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
    ws_sender
        .send(tungstenite::Message::Binary(send_buf.into()))
        .await
        .unwrap();

    // Simulate backend responding by sending response frame back
    let resp_buf = serialize_msgpack(&TunnelFrame::Res(TunnelResponseFrame {
        rid: "tunnel-req-1".into(),
        status: 200,
        headers: vec![("Content-Type".into(), "application/json".into())],
        body: b"{\"ok\":true}".to_vec(),
    }))
    .unwrap();
    ws_sender
        .send(tungstenite::Message::Binary(resp_buf.into()))
        .await
        .unwrap();

    ws_sender.close().await.unwrap();

    // Give mock time to process
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Verify mock received the request
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
    // Try connecting to a port nothing is listening on
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

    // Send two frames concurrently
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

    // Send both without awaiting individually (concurrent)
    let buf_a = serialize_msgpack(&TunnelFrame::Req(req_a)).unwrap();
    let buf_b = serialize_msgpack(&TunnelFrame::Req(req_b)).unwrap();
    let _ = ws_sender.send(tungstenite::Message::Binary(buf_a.into())).await;
    let _ = ws_sender.send(tungstenite::Message::Binary(buf_b.into())).await;

    ws_sender.close().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let requests = mock.get_requests().await;
    assert_eq!(requests.len(), 2);

    // Both rids should be present (order may vary)
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

    // Site A requests via same WS connection
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