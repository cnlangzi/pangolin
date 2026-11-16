//! Tunnel path integration tests.
//!
//! These tests exercise the **tunnel path through the proxy**
//! end-to-end: ngx routes a request whose `site.backend` has a
//! `tun_name:` prefix, opens a tunnel byte stream to the live tun
//! session, the tun dials the backend and copies the bytes, and the
//! response travels back the same way.
//!
//! Since issue #153 the tunnel is a **splice**: the wire format
//! inside the transport is a single `CONNECT <host>:<port>`
//! instruction followed by raw bytes — no frames, no per-stream
//! ids, no request re-serialisation. The transport-specific
//! end-to-end tests live in `tunnel_transport.rs`; here we keep a
//! couple of low-level sanity checks for protocol invariants on the
//! tunnel module.

use std::time::Duration;

use pangolin_core::tunnel::{
    bearer_token, compute_ws_accept, decode_connect, decode_connect_reply, encode_connect,
    encode_connect_reply, generate_ws_key, strip_hop_by_hop_headers,
};

use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

/// Hang a hanging TCP server for the per-conn timeout test.
async fn start_hanging_backend() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        loop {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                tokio::time::sleep(Duration::from_secs(300)).await;
            }
        }
    });
    addr
}

/// The connection instruction names the target by **host and port**,
/// so the tun can resolve the host in the operator's own network.
#[test]
fn connect_instruction_names_the_target_by_host_and_port() {
    let bytes = encode_connect("backend.internal", 8080);
    assert_eq!(bytes, b"CONNECT backend.internal:8080\r\n");
    let (host, port) = decode_connect(&bytes).unwrap();
    assert_eq!(host, "backend.internal");
    assert_eq!(port, 8080);
}

/// The tun's answer is `OK` or `ERR <reason>` — and an `ERR` becomes
/// a 502 for the client, so the reason has to survive.
#[test]
fn connect_reply_carries_the_failure_reason() {
    let ok = decode_connect_reply(&encode_connect_reply(Ok(()))).unwrap();
    assert_eq!(ok, Ok(()));
    let err = decode_connect_reply(&encode_connect_reply(Err("connection refused"))).unwrap();
    assert_eq!(err, Err("connection refused".to_string()));
}

/// strip_hop_by_hop_headers removes Connection, Keep-Alive,
/// Proxy-*, TE, Trailer, Transfer-Encoding, Upgrade.
#[test]
fn hop_by_hop_headers_are_stripped() {
    let mut headers = vec![
        ("Connection".into(), "close".into()),
        ("Keep-Alive".into(), "timeout=5".into()),
        ("Proxy-Authorization".into(), "Bearer x".into()),
        ("TE".into(), "trailers".into()),
        ("Trailer".into(), "Expires".into()),
        ("Transfer-Encoding".into(), "chunked".into()),
        ("Upgrade".into(), "h2c".into()),
        ("Host".into(), "app.example.com".into()),
        ("X-Custom".into(), "value".into()),
    ];
    strip_hop_by_hop_headers(&mut headers);
    let keys: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
    assert!(keys.contains(&"Host"));
    assert!(keys.contains(&"X-Custom"));
    assert!(!keys.contains(&"Connection"));
    assert!(!keys.contains(&"Keep-Alive"));
    assert!(!keys.contains(&"Proxy-Authorization"));
    assert!(!keys.contains(&"TE"));
    assert!(!keys.contains(&"Trailer"));
    assert!(!keys.contains(&"Transfer-Encoding"));
    assert!(!keys.contains(&"Upgrade"));
}

/// Bearer token extraction (Authorization: Bearer <token>).
#[test]
fn bearer_token_parsing() {
    assert_eq!(
        bearer_token(Some("Bearer secret")).as_deref(),
        Some("secret")
    );
    assert_eq!(
        bearer_token(Some("bearer secret")).as_deref(),
        Some("secret")
    );
    assert_eq!(
        bearer_token(Some("Bearer  spaced ")).as_deref(),
        Some("spaced")
    );
    assert_eq!(bearer_token(Some("Basic xyz")), None);
    assert_eq!(bearer_token(None), None);
    assert_eq!(bearer_token(Some("Bearer ")), None);
}

/// Sec-WebSocket-Accept computation (RFC 6455 §1.3).
/// The vector here is a known test value: key "dGhlIHNhbXBsZSBub25jZQ=="
/// accepts to "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=".
#[test]
fn ws_accept_is_rfc6455_compliant() {
    let accept = compute_ws_accept("dGhlIHNhbXBsZSBub25jZQ==");
    assert_eq!(accept, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
}

/// generate_ws_key produces 24-character base64 (16 random
/// bytes encoded).
#[test]
fn ws_key_is_24_chars_base64() {
    let key = generate_ws_key();
    assert_eq!(key.len(), 24);
    assert!(
        key.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=')
    );
}

/// Hanging backend exercises the tun-side reqwest timeout.
#[tokio::test]
async fn hanging_backend_times_out() {
    let backend_addr = start_hanging_backend().await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client should build");
    let uri = format!("http://{}/api/test", backend_addr);
    let req = client.get(&uri).build().expect("request should build");
    let start = std::time::Instant::now();
    let result = client.execute(req).await;
    let elapsed = start.elapsed();
    assert!(result.is_err(), "request to hanging backend should error");
    let err = result.unwrap_err();
    assert!(
        err.is_timeout() || err.is_connect(),
        "error should be timeout or connect error, got: {}",
        err
    );
    assert!(elapsed >= Duration::from_secs(2), "took {:?}", elapsed);
    assert!(elapsed < Duration::from_secs(5), "took {:?}", elapsed);
}
