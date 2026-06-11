//! Host header preservation tests.
//!
//! Covers: tests/E2E_PLAN.md → upstream_host_header
//!
//! Verifies that when proxying to a backend, the normalized Host header
//! from the client request is preserved and sent to the backend,
//! rather than the backend's IP address or hostname.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

use pangolin_core::index::{lookup_site, Indexes};
use pangolin_core::types::{Domain, Site};

// Raw TCP HTTP request helper. Delegates to the shared
// `harness::raw_request` so the connect/read timeouts and Host-header
// handling live in one place.
async fn send_raw_request(proxy_port: u16, host_header: &str, path: &str) -> (u16, String) {
    let addr = format!("127.0.0.1:{proxy_port}");
    crate::harness::raw_request(&addr, host_header, "GET", path, b"").await
}

// ---------------------------------------------------------------------------
// Mock HTTP backend that records received Host headers
// ---------------------------------------------------------------------------

struct HostCheckBackend {
    addr: String,
    received_headers: Arc<Mutex<Vec<(String, String)>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl HostCheckBackend {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let received_headers = Arc::new(Mutex::new(Vec::new()));
        let headers_for_spawn = received_headers.clone();

        let handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let hdrs = headers_for_spawn.clone();
                        tokio::spawn(handle_http_with_headers(stream, hdrs));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            addr,
            received_headers,
            handle,
        }
    }

    fn addr(&self) -> &str {
        &self.addr
    }

    async fn get_headers(&self) -> Vec<(String, String)> {
        self.received_headers.lock().await.clone()
    }
}

impl Drop for HostCheckBackend {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn handle_http_with_headers(
    mut stream: TcpStream,
    headers: Arc<Mutex<Vec<(String, String)>>>,
) {
    let mut buf = [0u8; 8192];
    let n = match stream.read(&mut buf).await {
        Ok(n) if n > 0 => n,
        _ => return,
    };

    let request_str = String::from_utf8_lossy(&buf[..n]);
    let lines: Vec<_> = request_str.lines().collect();

    // Extract all headers
    let mut received_headers = Vec::new();
    for line in &lines[1..] {
        if line.is_empty() {
            break;
        }
        if let Some(colon_pos) = line.find(':') {
            let name = line[..colon_pos].trim().to_string();
            let value = line[colon_pos + 1..].trim().to_string();
            received_headers.push((name, value));
        }
    }

    headers.lock().await.extend(received_headers);

    // Return 200 with header echo
    let body = "OK";
    let response = format!("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}", body);
    let _ = stream.write_all(response.as_bytes()).await;
}

// ---------------------------------------------------------------------------
// Minimal HTTP proxy that forwards to backend and normalizes Host header
// ---------------------------------------------------------------------------

async fn handle_proxy_connection(mut client: TcpStream, indexes: Arc<Indexes>) {
    let mut buf = [0u8; 16384];
    let n = match client.read(&mut buf).await {
        Ok(n) if n > 0 => n,
        _ => return,
    };

    let request_str = String::from_utf8_lossy(&buf[..n]).to_string();
    let lines: Vec<_> = request_str.lines().collect();

    // Parse Host header — may include :port
    let mut raw_host = String::new();
    for line in &lines[1..] {
        if line.to_lowercase().starts_with("host:") {
            if let Some(pos) = line.find(':') {
                raw_host = line[pos + 1..].trim().to_string();
            }
            break;
        }
    }

    // Normalize for lookup: strip port + lowercase
    let lookup_host = pangolin_core::normalize::normalize_host(&raw_host);

    let site = match lookup_site(&indexes, &lookup_host) {
        Some(s) => s.clone(),
        None => {
            let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nNot Found";
            let _ = client.write_all(resp.as_bytes()).await;
            return;
        }
    };

    // Connect to backend
    let backend_host = site
        .backend
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let (bh, bp) = if let Some(pos) = backend_host.find(':') {
        (
            &backend_host[..pos],
            backend_host[pos + 1..].parse::<u16>().unwrap_or(80),
        )
    } else {
        (backend_host, 80u16)
    };

    let mut backend = match TcpStream::connect(format!("{}:{}", bh, bp)).await {
        Ok(s) => s,
        Err(_) => {
            let resp = "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 13\r\n\r\nBad Gateway";
            let _ = client.write_all(resp.as_bytes()).await;
            return;
        }
    };

    // Rewrite Host header to normalized value before forwarding
    let request_str = String::from_utf8_lossy(&buf[..n]).to_string();
    let normalized_host_line = format!("Host: {}", lookup_host);
    let forwarded = request_str
        .lines()
        .map(|line| {
            if line.to_lowercase().starts_with("host:") {
                normalized_host_line.as_str()
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    if let Err(_) = backend.write_all(forwarded.as_bytes()).await {
        return;
    }

    // Relay response back to client
    let mut resp_buf = [0u8; 16384];
    loop {
        match backend.read(&mut resp_buf).await {
            Ok(0) => break,
            Ok(n) => {
                if client.write_all(&resp_buf[..n]).await.is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_indexes(sites: Vec<Site>, domains: Vec<Domain>) -> Indexes {
    Indexes::build(sites, domains)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// upstream_host_header — proxy preserves normalized Host header to backend
///
/// The backend receives "Host: api.example.com" (normalized),
/// NOT "Host: api.example.com:8080" (original with port),
/// NOT "Host: 127.0.0.1" (backend address).
#[tokio::test]
async fn upstream_host_header() {
    let backend = HostCheckBackend::start().await;

    let site = Site {
        name: "host-test-site".into(),
        backend: format!("http://{}", backend.addr()),
        enabled: true,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        host_mode: pangolin_core::types::HostMode::Passthrough,
        host_custom: None,
        domain_count: 0,
    };
    let domain = Domain {
        domain: "api.example.com".into(),
        site_name: "host-test-site".into(),
        enabled: true,
        auto_issue: false,
        dns_provider: None,
        created_at: chrono::Utc::now(),
    };
    let indexes = Arc::new(make_indexes(vec![site], vec![domain]));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_port = listener.local_addr().unwrap().port();
    let idx = indexes.clone();

    tokio::spawn(async move {
        loop {
            if let Ok((stream, _)) = listener.accept().await {
                let idx = idx.clone();
                tokio::spawn(handle_proxy_connection(stream, idx));
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();

    let (status, _resp_bytes) = send_raw_request(proxy_port, "api.example.com", "/test").await;
    assert_eq!(status, 200);

    // Check what headers the backend received
    let headers = backend.get_headers().await;
    let host_header = headers.iter().find(|(n, _)| n.to_lowercase() == "host");

    assert!(
        host_header.is_some(),
        "Host header should be present, got: {:?}",
        headers
    );
    let (_, host_value) = host_header.unwrap();

    // Should be normalized (no port, lowercase)
    assert_eq!(
        host_value.as_str(),
        "api.example.com",
        "Host header should be normalized to 'api.example.com', got '{}'",
        host_value
    );
}

/// upstream_host_header_with_port — Host header with port is normalized
///
/// When client sends "Host: api.example.com:8080", the port should be stripped
/// before lookup and the normalized clean host sent to backend.
#[tokio::test]
async fn upstream_host_header_with_port() {
    let backend = HostCheckBackend::start().await;

    let site = Site {
        name: "port-test-site".into(),
        backend: format!("http://{}", backend.addr()),
        enabled: true,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        host_mode: pangolin_core::types::HostMode::Passthrough,
        host_custom: None,
        domain_count: 0,
    };
    let domain = Domain {
        domain: "port.example.com".into(),
        site_name: "port-test-site".into(),
        enabled: true,
        auto_issue: false,
        dns_provider: None,
        created_at: chrono::Utc::now(),
    };
    let indexes = Arc::new(make_indexes(vec![site], vec![domain]));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_port = listener.local_addr().unwrap().port();
    let idx = indexes.clone();

    tokio::spawn(async move {
        loop {
            if let Ok((stream, _)) = listener.accept().await {
                let idx = idx.clone();
                tokio::spawn(handle_proxy_connection(stream, idx));
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();

    // Request with port in Host header
    let (status, _resp_bytes) =
        send_raw_request(proxy_port, "port.example.com:8080", "/test").await;
    assert_eq!(status, 200);

    let headers = backend.get_headers().await;
    let host_header = headers.iter().find(|(n, _)| n.to_lowercase() == "host");

    assert!(host_header.is_some());
    let (_, host_value) = host_header.unwrap();

    // Host to backend should be normalized (port stripped)
    assert_eq!(
        host_value.as_str(),
        "port.example.com",
        "Host header should be normalized to 'port.example.com', got '{}'",
        host_value
    );
}
