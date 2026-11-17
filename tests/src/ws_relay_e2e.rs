//! WebSocket relay integration tests.
//!
//! Since issue #153 the tunnel is a **splice**: the gateway's pingora
//! writes the client's request bytes into a tunnel stream and the tun
//! copies them to the backend. The copy is
//! `pangolin_tun::relay_bidirectional`, and its
//! **half-close** behaviour is what makes a long-lived response
//! survive the end of the request.
//!
//! The end-to-end happy-path tests for the relay live in
//! `real_e2e::ws_*` (which spawn the real `pangolin-ngx` and
//! `pangolin-tun` binaries). This file keeps the low-level tests for
//! the half-close pump's invariants.

use std::time::Duration;

use pangolin_tun::relay_bidirectional;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Half-close: closing one direction of the request side must only
/// shut down the *write* side of the response side — the other
/// direction keeps flowing.
///
/// The relay runs on `(a, b)`; `a_peer` stands in for the gateway and
/// `b_peer` for the backend.
#[tokio::test]
async fn half_close_only_propagates_one_direction() {
    let (mut a, mut a_peer) = tokio::io::duplex(64 * 1024);
    let (mut b, mut b_peer) = tokio::io::duplex(64 * 1024);

    let relay = tokio::spawn(async move {
        relay_bidirectional(&mut a, &mut b).await.unwrap();
    });

    // The request side writes and then half-closes.
    a_peer.write_all(b"hello").await.unwrap();
    a_peer.shutdown().await.unwrap();

    // The bytes arrive on the backend side …
    let mut buf = [0u8; 8];
    let n = b_peer.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"hello", "the request bytes must reach the peer");

    // … and the backend side can still answer: the relay only shut
    // down the *write* half of the backend side, so a response that
    // arrives after the request ended still flows back.
    b_peer.write_all(b"world").await.unwrap();

    let mut back = Vec::new();
    let mut chunk = [0u8; 8];
    while back.len() < 5 {
        let n = a_peer.read(&mut chunk).await.unwrap();
        assert!(n > 0, "the relay must keep the response direction open");
        back.extend_from_slice(&chunk[..n]);
    }
    assert_eq!(&back[..5], b"world");

    relay.abort();
}

/// The relay returns once both directions are done — it must not spin
/// forever after the peer closed.
#[tokio::test]
async fn relay_completes_on_close() {
    let (mut a, mut a_peer) = tokio::io::duplex(64 * 1024);
    let (mut b, mut b_peer) = tokio::io::duplex(64 * 1024);

    let relay = tokio::spawn(async move {
        relay_bidirectional(&mut a, &mut b).await.unwrap();
    });

    // The backend side answers and closes.
    b_peer.write_all(b"hello").await.unwrap();
    b_peer.shutdown().await.unwrap();
    // The request side closes too.
    a_peer.shutdown().await.unwrap();

    tokio::time::timeout(Duration::from_secs(2), relay)
        .await
        .expect("the relay must finish once both directions are done")
        .expect("the relay task must not panic");
}
