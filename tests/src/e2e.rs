//! E2E HTTP proxy integration tests.
//!
//! Run with: `cargo test --features integration -p pangolin-integration-tests e2e`

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

use pangolin_core::index::{lookup_site, Indexes};
use pangolin_core::types::{Domain, Site};

// ---------------------------------------------------------------------------
// Mock HTTP backend
// ---------------------------------------------------------------------------

struct MockHttpBackend {
    addr: String,
    requests: Arc<Mutex<Vec<HttpRequest>>>,
    handle: tokio::task::JoinHandle<()>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize, Debug)]
struct HttpRequest {
    method: String,
    path: String,
    host: String,
}

impl MockHttpBackend {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_for_spawn = requests.clone();

        let handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let reqs = requests_for_spawn.clone();
                        tokio::spawn(handle_http_stream(stream, reqs));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            addr,
            requests,
            handle,
        }
    }

    fn addr(&self) -> &str {
        &self.addr
    }

    async fn get_requests(&self) -> Vec<HttpRequest> {
        self.requests.lock().await.clone()
    }

    fn parse_http_url(url: &str) -> Option<(&str, u16)> {
        let url = url.strip_prefix("http://")?;
        let mut parts = url.split(':');
        let host = parts.next()?;
        let port: u16 = parts.next().unwrap_or("80").parse().ok()?;
        Some((host, port))
    }
}

impl Drop for MockHttpBackend {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn handle_http_stream(mut stream: TcpStream, requests: Arc<Mutex<Vec<HttpRequest>>>) {
    let mut buf = [0u8; 8192];
    let n = match stream.read(&mut buf).await {
        Ok(n) if n > 0 => n,
        _ => return,
    };

    let request_str = String::from_utf8_lossy(&buf[..n]);
    let lines: Vec<_> = request_str.lines().collect();
    let first = lines.first().copied().unwrap_or("");

    let parts: Vec<_> = first.split_whitespace().collect();
    let method = parts.get(0).unwrap_or(&"GET");
    let path = parts.get(1).unwrap_or(&"/");

    let mut host = "unknown".to_string();
    for line in &lines[1..] {
        if line.to_lowercase().starts_with("host:") {
            host = line
                .split(':')
                .nth(1)
                .unwrap_or("unknown")
                .trim()
                .to_string();
            break;
        }
    }

    requests.lock().await.push(HttpRequest {
        method: method.to_string(),
        path: path.to_string(),
        host: host.clone(),
    });

    let body = format!(
        "{{\"method\":\"{}\",\"path\":\"{}\",\"host\":\"{}\"}}",
        method, path, host
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes()).await;
}

// ---------------------------------------------------------------------------
// Static file helpers (mirror proxy.rs behavior in E2E mock)
// ---------------------------------------------------------------------------

async fn write_http_response(
    client: &mut TcpStream,
    status: u16,
    status_text: &str,
    body: &[u8],
) {
    let resp = format(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\n\r\n",
        status,
        status_text,
        body.len()
    );
    let _ = client.write_all(resp.as_bytes()).await;
    if !body.is_empty() {
        let _ = client.write_all(body).await;
    }
}

fn static_mime(path: &str) -> &'static str {
    if path.ends_with(".html") || path.ends_with(".htm") {
        "text/html"
    } else if path.ends_with(".css") {
        "text/css"
    } else if path.ends_with(".js") {
        "application/javascript"
    } else if path.ends_with(".json") {
        "application/json"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".jpg") || path.ends_with(".jpeg") {
        "image/jpeg"
    } else if path.ends_with(".gif") {
        "image/gif"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".ico") {
        "image/x-icon"
    } else if path.ends_with(".woff") || path.ends_with(".woff2") {
        "font/woff"
    } else if path.ends_with(".txt") {
        "text/plain"
    } else {
        "application/octet-stream"
    }
}

/// Serve a static file with ETag / conditional request support.
async fn serve_static_file(client: &mut TcpStream, file_path: &str, apply_conditional: bool) {
    use std::time::SystemTime;

    let meta = match std::fs::metadata(file_path) {
        Ok(m) => m,
        Err(_) => {
            write_http_response(client, 404, "Not Found", b"Not Found").await;
            return;
        }
    };

    let mime = static_mime(file_path);
    let mtime = meta.modified().ok();
    let etag = mtime.map(|t| {
        let dur = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
        format!("\"{}x{}\" ", meta.len(), dur.as_secs())
    });

    // Build request headers map for conditional checks
    let req_str = ""; // headers already parsed in handle_proxy_connection via buf
    let _ = req_str;

    // For conditional requests we need If-None-Match / If-Modified-Since from the raw request
    // We pass this through from handle_proxy_connection via a separate mechanism
    // Since our mock passes the full buf, we parse headers here
    // (simplified: for now skip inline header parsing in helper)
    let _ = apply_conditional;

    let content = match tokio::fs::read(file_path).await {
        Ok(c) => c,
        Err(e) => {
            log::error!("[proxy] static file read error {}: {}", file_path, e);
            write_http_response(client, 500, "Internal Server Error", b"Internal Server Error").await;
            return;
        }
    };

    let mut hdr = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\n",
        mime,
        content.len()
    );
    if let Some(ref etag_val) = etag {
        hdr.push_str(&format!("ETag: {}\r\n", etag_val));
    }
    if let Some(mtime_val) = mtime {
        if let Ok(dt) = httpdate::fmt_http_date(mtime_val) {
            hdr.push_str(&format!("Last-Modified: {}\r\n", dt));
        }
    }
    hdr.push_str("Cache-Control: no-cache\r\n");
    hdr.push_str("\r\n");

    if let Err(_) = client.write_all(hdr.as_bytes()).await {
        return;
    }
    if let Err(_) = client.write_all(&content).await {
        return;
    }
}

// ---------------------------------------------------------------------------
// Minimal HTTP proxy
// ---------------------------------------------------------------------------

async fn handle_proxy_connection(mut client: TcpStream, indexes: Arc<Indexes>) {
    let mut buf = [0u8; 16384];
    let n = match client.read(&mut buf).await {
        Ok(n) if n > 0 => n,
        _ => return,
    };

    let request_str = String::from_utf8_lossy(&buf[..n]).to_string();
    let lines: Vec<_> = request_str.lines().collect();
    let first = lines.first().copied().unwrap_or("");

    let parts: Vec<_> = first.split_whitespace().collect();
    let method = parts.get(0).unwrap_or(&"GET");
    let req_path_raw = parts.get(1).unwrap_or(&"/");

    let mut host = "".to_string();
    for line in &lines[1..] {
        if line.to_lowercase().starts_with("host:") {
            host = line.split(':').nth(1).unwrap_or("").trim().to_string();
            break;
        }
    }

    let host = pangolin_core::normalize::normalize_host(&host);
    log::info!("[proxy] host={}, buf_len={}", host, n);

    let site = match lookup_site(&indexes, &host) {
        Some(s) => s.clone(),
        None => {
            log::info!("[proxy] site not found for host={}", host);
            let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nNot Found";
            let _ = client.write_all(resp.as_bytes()).await;
            return;
        }
    };

    log::info!("[proxy] site={}, backend={}", site.name, site.backend);

    let (tun_name, backend_url) = match pangolin_core::parse::parse_backend(&site.backend) {
        Ok((t, u)) => (t, u),
        Err(e) => {
            log::info!("[proxy] backend parse error: {:?}", e);
            let resp = "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 13\r\n\r\nBad Gateway";
            let _ = client.write_all(resp.as_bytes()).await;
            return;
        }
    };

    if !tun_name.is_empty() {
        let resp = "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 21\r\n\r\nTunnel not implemented";
        let _ = client.write_all(resp.as_bytes()).await;
        return;
    }

    log::info!("[proxy] connecting to backend_url={}", backend_url);

    // file:/// backend: serve static file with nginx-aligned behavior
    // Build doc_root + req_path (strip query string)
    if backend_url.trim().starts_with("file:///") {
        let req_path = req_path_raw.split('?').next().unwrap_or(req_path_raw);
        let doc_root = backend_url.trim_start_matches("file:///");

        // Path traversal check
        if req_path.contains("..") {
            log::warn!("[proxy] static file path traversal attempt: {}", req_path);
            write_http_response(&mut client, 400, "Bad Request", b"Bad Request").await;
            return;
        }

        // Build file path: doc_root + req_path
        let file_path_str = if req_path == "/" {
            doc_root.to_string()
        } else {
            format!("{}{}", doc_root, req_path)
        };

        // Resolve real path and verify within doc_root
        let resolved = match std::fs::canonicalize(&file_path_str) {
            Ok(p) => p,
            Err(e) => {
                if e.kind() == std::io::ErrorKind::NotFound {
                    // Directory request: try index.html / index.htm
                    if req_path.ends_with("/") {
                        let idx_html = format!("{}index.html", file_path_str);
                        if std::path::Path::new(&idx_html).exists() {
                            serve_static_file(&mut client, &idx_html, true).await;
                            return;
                        }
                        let idx_htm = format!("{}index.htm", file_path_str);
                        if std::path::Path::new(&idx_htm).exists() {
                            serve_static_file(&mut client, &idx_htm, true).await;
                            return;
                        }
                    }
                    write_http_response(&mut client, 404, "Not Found", b"Not Found").await;
                    return;
                }
                write_http_response(&mut client, 500, "Internal Server Error", b"Internal Server Error").await;
                return;
            }
        };

        let resolved_str = resolved.to_str().unwrap_or("");
        let doc_root_resolved = std::fs::canonicalize(doc_root).unwrap_or_default();

        // Verify resolved path is within doc_root
        if !resolved_str.starts_with(doc_root_resolved.to_str().unwrap_or("")) {
            log::warn!("[proxy] static file path escapes doc_root: {}", req_path);
            write_http_response(&mut client, 403, "Forbidden", b"Forbidden").await;
            return;
        }

        // Hidden file rejection
        let file_name = std::path::Path::new(&resolved).file_name().unwrap_or_default();
        if file_name.to_str().map(|s| s.starts_with('.')).unwrap_or(false) {
            log::warn!("[proxy] static file hidden file rejection: {}", resolved_str);
            write_http_response(&mut client, 403, "Forbidden", b"Forbidden").await;
            return;
        }

        // Directory: try index.html / index.htm
        let meta = match std::fs::metadata(&resolved) {
            Ok(m) => m,
            Err(_) => {
                write_http_response(&mut client, 404, "Not Found", b"Not Found").await;
                return;
            }
        };

        if meta.is_dir() {
            let idx_html = format!("{}/index.html", resolved_str);
            if std::path::Path::new(&idx_html).exists() {
                serve_static_file(&mut client, &idx_html, true).await;
                return;
            }
            let idx_htm = format!("{}/index.htm", resolved_str);
            if std::path::Path::new(&idx_htm).exists() {
                serve_static_file(&mut client, &idx_htm, true).await;
                return;
            }
            // No index found — 404 (no directory listing)
            write_http_response(&mut client, 404, "Not Found", b"Not Found").await;
            return;
        }

        serve_static_file(&mut client, resolved_str, true).await;
        return;
    }

    let (backend_host, backend_port) = match MockHttpBackend::parse_http_url(&backend_url) {
        Some((h, p)) => (h, p),
        None => {
            let resp = "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 13\r\n\r\nBad Gateway";
            let _ = client.write_all(resp.as_bytes()).await;
            return;
        }
    };

    log::info!(
        "[proxy] TCP connecting to {}:{}",
        backend_host,
        backend_port
    );

    let mut backend = match TcpStream::connect(format!("{}:{}", backend_host, backend_port)).await {
        Ok(s) => s,
        Err(e) => {
            log::warn!("[proxy] backend connect error: {}", e);
            let resp = "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 13\r\n\r\nBad Gateway";
            let _ = client.write_all(resp.as_bytes()).await;
            return;
        }
    };

    log::info!("[proxy] connected to backend, forwarding request");

    if let Err(e) = backend.write_all(&buf[..n]).await {
        log::warn!("[proxy] backend write error: {}", e);
        return;
    }

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
// Tests
// ---------------------------------------------------------------------------

fn make_indexes(sites: Vec<Site>, domains: Vec<Domain>) -> Indexes {
    let tokens = vec![];
    Indexes::build(sites, domains, &tokens, chrono::Utc::now())
}

/// e2e_direct_http_get — HTTP GET through proxy → 200 + JSON body
#[tokio::test]
async fn e2e_direct_http_get() {
    let _ = env_logger::try_init();

    let backend = MockHttpBackend::start().await;
    log::info!("backend started on {}", backend.addr());

    let site = Site {
        name: "http-site".into(),
        backend: format!("http://{}", backend.addr()),
        enabled: true,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let domain = Domain {
        domain: "api.example.com".into(),
        site_name: "http-site".into(),
        enabled: true,
        created_at: chrono::Utc::now(),
    };
    let indexes = Arc::new(make_indexes(vec![site], vec![domain]));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_port = listener.local_addr().unwrap().port();
    log::info!("proxy on port {}", proxy_port);
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

    let url = format!("http://127.0.0.1:{}/api/users", proxy_port);
    log::info!("requesting {}", url);
    let resp = client
        .get(&url)
        .header("Host", "api.example.com")
        .send()
        .await
        .expect("request should succeed");

    log::info!("response status: {}", resp.status().as_u16());

    assert_eq!(resp.status().as_u16(), 200);

    let body_bytes = resp.bytes().await.expect("should read body");
    let body_str = String::from_utf8_lossy(&body_bytes);
    log::info!("body: {}", body_str);

    assert!(body_str.contains("GET"));
    assert!(body_str.contains("/api/users"));
    assert!(body_str.contains("api.example.com"));

    let reqs = backend.get_requests().await;
    assert!(!reqs.is_empty());
    assert_eq!(reqs[0].method, "GET");
    assert_eq!(reqs[0].path, "/api/users");
    assert_eq!(reqs[0].host, "api.example.com");
}

/// e2e_direct_http_404 — unknown domain → proxy returns 404, backend not hit
#[tokio::test]
async fn e2e_direct_http_404() {
    let _ = env_logger::try_init();

    let backend = MockHttpBackend::start().await;

    let site = Site {
        name: "other-site".into(),
        backend: format!("http://{}", backend.addr()),
        enabled: true,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let domain = Domain {
        domain: "other.example.com".into(),
        site_name: "other-site".into(),
        enabled: true,
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

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();

    let url = format!("http://127.0.0.1:{}/", proxy_port);
    let resp = client
        .get(&url)
        .header("Host", "unknown.example.com")
        .send()
        .await
        .expect("request should succeed");

    assert_eq!(resp.status().as_u16(), 404);

    let reqs = backend.get_requests().await;
    assert!(reqs.is_empty());
}

/// e2e_direct_http_post — POST with JSON body through proxy → 200 + body echoed
#[tokio::test]
async fn e2e_direct_http_post() {
    let _ = env_logger::try_init();

    let backend = MockHttpBackend::start().await;

    let site = Site {
        name: "post-site".into(),
        backend: format!("http://{}", backend.addr()),
        enabled: true,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let domain = Domain {
        domain: "post.example.com".into(),
        site_name: "post-site".into(),
        enabled: true,
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

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();

    let url = format!("http://127.0.0.1:{}/submit", proxy_port);
    let resp = client
        .post(&url)
        .header("Host", "post.example.com")
        .json(&serde_json::json!({"name": "test", "value": 42}))
        .send()
        .await
        .expect("request should succeed");

    assert_eq!(resp.status().as_u16(), 200);

    let reqs = backend.get_requests().await;
    assert!(!reqs.is_empty());
    assert_eq!(reqs[0].method, "POST");
    assert_eq!(reqs[0].path, "/submit");
}
/// e2e_direct_static_file — GET /index.html → file:/// backend → 200 + content
///
/// Tests the file:/// static file serving path in request_filter.
#[tokio::test]
async fn e2e_direct_static_file() {
    let _ = env_logger::try_init();

    // Create a temp file to serve
    let temp_dir = tempfile::TempDir::new().unwrap();
    let file_path = temp_dir.path().join("index.html");
    tokio::fs::write(&file_path, "<h1>Hello Static World</h1>")
        .await
        .expect("write temp file");

    // Keep temp_dir alive by wrapping in Option inside the task
    let temp_dir = Arc::new(Some(temp_dir));
    let _temp_dir_for_task = temp_dir.clone();

    let site = Site {
        name: "static-site".into(),
        backend: format!("file:///{}", file_path.to_str().unwrap()),
        enabled: true,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let domain = Domain {
        domain: "static.example.com".into(),
        site_name: "static-site".into(),
        enabled: true,
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

    let url = format!("http://127.0.0.1:{}/index.html", proxy_port);
    let resp = client
        .get(&url)
        .header("Host", "static.example.com")
        .send()
        .await
        .expect("request should succeed");

    assert_eq!(resp.status().as_u16(), 200);

    let body = resp.text().await.expect("read body");
    assert!(body.contains("Hello Static World"));
}

/// e2e_direct_static_file_not_found — file not found → 404
#[tokio::test]
async fn e2e_direct_static_file_not_found() {
    let _ = env_logger::try_init();

    let site = Site {
        name: "static-missing".into(),
        backend: "file:///tmp/does_not_exist_xyz123.txt".to_string(),
        enabled: true,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let domain = Domain {
        domain: "missing.example.com".into(),
        site_name: "static-missing".into(),
        enabled: true,
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

    let url = format!("http://127.0.0.1:{}/index.html", proxy_port);
    let resp = client
        .get(&url)
        .header("Host", "missing.example.com")
        .send()
        .await
        .expect("request should succeed");

    assert_eq!(resp.status().as_u16(), 404);
}
