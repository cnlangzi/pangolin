//! The edge cache, end to end (issue #259).
//!
//! The tests drive the **real** gateway binary against a mock backend
//! that counts the requests it receives, so every assertion about the
//! origin's request count is an assertion about what the cache did.
//!
//! The cache's own rules live in `docs/design/cache.md`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rusqlite::Connection;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::harness::{
    NgxProcess, TunProcess, admin_session_cookie, init_pangolin_db, read_sse_data_frames,
};

// ---------------------------------------------------------------------------
// Mock backend
// ---------------------------------------------------------------------------

/// What the backend saw of one request.
#[derive(Debug, Clone, Default)]
struct Seen {
    path: String,
    conditional: bool,
}

/// A backend that serves CSS files and counts the requests it got.
///
/// The path decides what the backend answers, so a test can drive the
/// cache's rules one response at a time.
struct CountingBackend {
    addr: String,
    requests: Arc<AtomicUsize>,
    seen: Arc<tokio::sync::Mutex<Vec<Seen>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl CountingBackend {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        let requests = Arc::new(AtomicUsize::new(0));
        let seen: Arc<tokio::sync::Mutex<Vec<Seen>>> =
            Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let per_path: Arc<tokio::sync::Mutex<HashMap<String, usize>>> =
            Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let counter = requests.clone();
        let seen_for_task = seen.clone();
        let per_path_for_task = per_path.clone();
        let handle = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                let seen = seen_for_task.clone();
                let per_path = per_path_for_task.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let path = request
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_string();
                    let conditional = request.to_ascii_lowercase().contains("if-none-match:");
                    let count = {
                        let mut counts = per_path.lock().await;
                        let entry = counts.entry(path.clone()).or_insert(0);
                        *entry += 1;
                        *entry
                    };
                    seen.lock().await.push(Seen {
                        path: path.clone(),
                        conditional,
                    });
                    let response = answer(&path, conditional, count);
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        Self {
            addr,
            requests,
            seen,
            handle,
        }
    }

    /// How many requests the backend has seen.
    fn count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    /// What the backend saw.
    async fn seen(&self) -> Vec<Seen> {
        self.seen.lock().await.clone()
    }
}

impl Drop for CountingBackend {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// The response the backend answers with.
///
/// The paths are the cache's rules, one response each. `count` is how
/// many requests the backend saw for this path.
fn answer(path: &str, conditional: bool, count: usize) -> String {
    // The flaky asset: the origin works the first time and fails
    // afterwards, so the cache has to serve the stale answer.
    if path == "/flaky.css" && count > 1 && !conditional {
        return "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_string();
    }
    // A conditional request is answered with "not modified" — the
    // cached body is reused.
    if conditional {
        return "HTTP/1.1 304 Not Modified\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n"
            .to_string();
    }
    let body = "body{color:red}";
    let content_type = if path == "/events.css" {
        "text/event-stream"
    } else {
        "text/css"
    };
    let mut headers: Vec<(&str, String)> = vec![
        ("Content-Type", content_type.to_string()),
        ("Content-Length", body.len().to_string()),
        ("ETag", "\"v1\"".to_string()),
        // The origin says the response varies on the language. The
        // cache does not listen to that: it varies on the encoding
        // and on nothing else.
        ("Vary", "Accept-Language".to_string()),
    ];
    match path {
        "/nostore.css" => headers.push(("Cache-Control", "no-store".to_string())),
        "/private.css" => headers.push(("Cache-Control", "private".to_string())),
        "/cookie.css" => headers.push(("Set-Cookie", "sid=1".to_string())),
        // Stored, but stale from the moment it lands: the next read
        // revalidates it with a conditional request.
        "/revalidate.css" => headers.push(("Cache-Control", "no-cache".to_string())),
        // Fresh for nothing, but servable when the origin fails.
        "/flaky.css" => headers.push(("Cache-Control", "max-age=0, stale-if-error=60".to_string())),
        _ => {}
    }
    let mut response = String::from("HTTP/1.1 200 OK\r\n");
    for (name, value) in &headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str("Connection: close\r\n\r\n");
    response.push_str(body);
    response
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn seed_site(conn: &Connection, name: &str, backend: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO sites (name, backend, enabled, host_mode, host_custom, created_at, updated_at) \
         VALUES (?1, ?2, 1, 'passthrough', NULL, ?3, ?3)",
        rusqlite::params![name, backend, now],
    )
    .expect("insert site");
}

fn seed_domain(conn: &Connection, domain: &str, site_name: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO domains (domain, site_name, enabled, created_at) VALUES (?1, ?2, 1, ?3)",
        rusqlite::params![domain, site_name, now],
    )
    .expect("insert domain");
}

fn seed_tun(conn: &Connection, name: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    let hash = pangolin_core::db::sha256_hex("test-token");
    conn.execute(
        "INSERT INTO tun (name, token, token_hash, enabled, online, registered_at, last_seen_at, expires_at) \
         VALUES (?1, 'test-token', ?2, 1, 0, ?3, ?3, NULL)",
        rusqlite::params![name, hash, now],
    )
    .expect("insert tun");
}

/// The `[cache]` section a test writes into `ngx.yml`.
///
/// `min_uses: 1` makes the first request store the object, so a test
/// that is about the cache's *behaviour* does not have to warm the
/// cache up first. The hotness threshold has its own test.
fn cache_config(min_uses: u32) -> String {
    format!("cache:\n  enabled: true\n  dir: ./cache\n  min_uses: {min_uses}\n  ttl: 3600\n")
}

/// Start a gateway with a site that serves the mock backend.
async fn start_gateway(backend: &CountingBackend, cache: &str) -> NgxProcess {
    NgxProcess::start_with_config(
        |db_path| {
            init_pangolin_db(db_path);
            let conn = Connection::open(db_path).expect("open db");
            seed_site(&conn, "cache-site", &format!("http://{}", backend.addr));
            seed_domain(&conn, "cache.test", "cache-site");
        },
        cache,
    )
    .await
}

/// A raw request that returns the **whole** response, headers
/// included.
///
/// The shared harness helper returns the body only, and the cache's
/// `X-Cache` and `Age` headers are what these tests read.
async fn request(
    addr: &str,
    host: &str,
    method: &str,
    path: &str,
    extra_headers: &[(&str, &str)],
) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    for (name, value) in extra_headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .await
        .expect("write request");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read response");
    String::from_utf8_lossy(&buf).into_owned()
}

/// The value of a response header, if the response carries it.
fn header(response: &str, name: &str) -> Option<String> {
    let head = response.split("\r\n\r\n").next().unwrap_or("");
    head.lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .find(|(header, _)| header.trim().eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim().to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The second request for a static asset is answered from the cache.
///
/// The contract: the first request is a miss and goes to the origin,
/// the second one is a hit and does **not** — and it carries the
/// `Age` header, so a client can tell how old the answer is.
#[tokio::test]
async fn cache_e2e_the_second_request_is_served_from_the_cache() {
    let backend = CountingBackend::start().await;
    let ngx = start_gateway(&backend, &cache_config(1)).await;
    let addr = format!("127.0.0.1:{}", ngx.http_port);

    let response = request(&addr, "cache.test", "GET", "/a.css", &[]).await;
    assert!(response.contains("200 OK"), "{response}");
    assert!(response.ends_with("body{color:red}"), "{response}");

    let response = request(&addr, "cache.test", "GET", "/a.css", &[]).await;
    assert!(response.contains("200 OK"), "{response}");
    assert!(
        response.ends_with("body{color:red}"),
        "the cached body must be the same: {response}"
    );
    assert_eq!(
        backend.count(),
        1,
        "the second request must not reach the origin"
    );
}

/// The response says where it came from.
///
/// `X-Cache: MISS` on the first request and `X-Cache: HIT` on the
/// second — the header is what makes the cache observable.
#[tokio::test]
async fn cache_e2e_the_response_says_whether_it_came_from_the_cache() {
    let backend = CountingBackend::start().await;
    let ngx = start_gateway(&backend, &cache_config(1)).await;
    let addr = format!("127.0.0.1:{}", ngx.http_port);

    let response = request(&addr, "cache.test", "GET", "/a.css", &[]).await;
    assert_eq!(
        header(&response, "X-Cache").as_deref(),
        Some("MISS"),
        "{response}"
    );

    let response = request(&addr, "cache.test", "GET", "/a.css", &[]).await;
    assert_eq!(
        header(&response, "X-Cache").as_deref(),
        Some("HIT"),
        "{response}"
    );
    assert!(
        header(&response, "Age").is_some(),
        "a hit must carry an Age header: {response}"
    );
}

/// The hotness threshold: a key that is not asked for often enough is
/// not stored.
///
/// With `min_uses: 2` the first request is not stored, the second one
/// creates the entry, and the third one is a hit.
#[tokio::test]
async fn cache_e2e_a_cold_key_is_not_stored() {
    let backend = CountingBackend::start().await;
    let ngx = start_gateway(&backend, &cache_config(2)).await;
    let addr = format!("127.0.0.1:{}", ngx.http_port);

    let response = request(&addr, "cache.test", "GET", "/a.css", &[]).await;
    assert_eq!(
        header(&response, "X-Cache").as_deref(),
        Some("MISS"),
        "{response}"
    );
    assert_eq!(backend.count(), 1);

    let response = request(&addr, "cache.test", "GET", "/a.css", &[]).await;
    assert_eq!(
        header(&response, "X-Cache").as_deref(),
        Some("MISS"),
        "{response}"
    );
    assert_eq!(backend.count(), 2, "the second request still goes out");

    let response = request(&addr, "cache.test", "GET", "/a.css", &[]).await;
    assert_eq!(
        header(&response, "X-Cache").as_deref(),
        Some("HIT"),
        "{response}"
    );
    assert_eq!(
        backend.count(),
        2,
        "the third request is served from the cache"
    );
}

/// The query string is not part of the key.
///
/// `/a.css?v=1` and `/a.css?v=2` are the same asset: the cache stores
/// one entry for the path, and the second request does not reach the
/// origin.
#[tokio::test]
async fn cache_e2e_the_query_string_is_not_part_of_the_key() {
    let backend = CountingBackend::start().await;
    let ngx = start_gateway(&backend, &cache_config(1)).await;
    let addr = format!("127.0.0.1:{}", ngx.http_port);

    let _ = request(&addr, "cache.test", "GET", "/a.css?v=1", &[]).await;
    assert_eq!(backend.count(), 1);

    let response = request(&addr, "cache.test", "GET", "/a.css?v=2", &[]).await;
    assert_eq!(
        header(&response, "X-Cache").as_deref(),
        Some("HIT"),
        "{response}"
    );
    assert_eq!(
        backend.count(),
        1,
        "the other query must hit the same entry"
    );
}

/// The cache varies on the encoding, and on nothing else.
///
/// A gzip request and a br request are two variants: the second one
/// does not read the first one's entry.
#[tokio::test]
async fn cache_e2e_the_encoding_is_a_variant_of_its_own() {
    let backend = CountingBackend::start().await;
    let ngx = start_gateway(&backend, &cache_config(1)).await;
    let addr = format!("127.0.0.1:{}", ngx.http_port);

    let _ = request(
        &addr,
        "cache.test",
        "GET",
        "/a.css",
        &[("Accept-Encoding", "gzip")],
    )
    .await;
    assert_eq!(backend.count(), 1);

    // The same encoding again: a hit.
    let response = request(
        &addr,
        "cache.test",
        "GET",
        "/a.css",
        &[("Accept-Encoding", "gzip")],
    )
    .await;
    assert_eq!(
        header(&response, "X-Cache").as_deref(),
        Some("HIT"),
        "{response}"
    );
    assert_eq!(backend.count(), 1);

    // Another encoding: its own variant, so it goes to the origin.
    let response = request(
        &addr,
        "cache.test",
        "GET",
        "/a.css",
        &[("Accept-Encoding", "br")],
    )
    .await;
    assert_eq!(
        header(&response, "X-Cache").as_deref(),
        Some("MISS"),
        "{response}"
    );
    assert_eq!(backend.count(), 2, "the other encoding is another variant");
}

/// The origin's own `Vary` declaration takes no part in the cache.
///
/// The backend answers with `Vary: Accept-Language`, and the cache
/// does not listen to it: two requests in two languages are one
/// entry, not two.
#[tokio::test]
async fn cache_e2e_the_origins_vary_declaration_is_not_a_variant() {
    let backend = CountingBackend::start().await;
    let ngx = start_gateway(&backend, &cache_config(1)).await;
    let addr = format!("127.0.0.1:{}", ngx.http_port);

    let _ = request(
        &addr,
        "cache.test",
        "GET",
        "/a.css",
        &[("Accept-Language", "en")],
    )
    .await;
    assert_eq!(backend.count(), 1);

    let response = request(
        &addr,
        "cache.test",
        "GET",
        "/a.css",
        &[("Accept-Language", "de")],
    )
    .await;
    assert_eq!(
        header(&response, "X-Cache").as_deref(),
        Some("HIT"),
        "the origin's Vary must not split the entry: {response}"
    );
    assert_eq!(backend.count(), 1);
}

/// A request that carries a session bypasses the cache.
///
/// The backend may answer a request that carries a cookie with
/// something that is not the same for every visitor, so such a
/// request is never served from the cache and never stored in it.
#[tokio::test]
async fn cache_e2e_a_request_with_a_cookie_bypasses_the_cache() {
    let backend = CountingBackend::start().await;
    let ngx = start_gateway(&backend, &cache_config(1)).await;
    let addr = format!("127.0.0.1:{}", ngx.http_port);

    for header_value in [("Cookie", "session=abc"), ("Authorization", "Bearer t")] {
        let _ = request(&addr, "cache.test", "GET", "/a.css", &[header_value]).await;
        let _ = request(&addr, "cache.test", "GET", "/a.css", &[header_value]).await;
    }
    assert_eq!(
        backend.count(),
        4,
        "a request with a cookie or an authorization must always reach the origin"
    );
}

/// Only the whitelisted extensions are cached.
///
/// A page (`/index.html`) is not a static asset, so it is never
/// stored — the whitelist is the guard that says "the answer of this
/// path does not depend on the visitor".
#[tokio::test]
async fn cache_e2e_a_path_outside_the_whitelist_is_not_cached() {
    let backend = CountingBackend::start().await;
    let ngx = start_gateway(&backend, &cache_config(1)).await;
    let addr = format!("127.0.0.1:{}", ngx.http_port);

    let response = request(&addr, "cache.test", "GET", "/index.html", &[]).await;
    assert!(
        header(&response, "X-Cache").is_none(),
        "a path outside the whitelist must not be cached: {response}"
    );
    let response = request(&addr, "cache.test", "GET", "/index.html", &[]).await;
    assert!(header(&response, "X-Cache").is_none(), "{response}");
    assert_eq!(backend.count(), 2);
}

/// The responses the origin refuses to share are not stored.
///
/// A response that sets a cookie, one that says `no-store`, one that
/// says `private` and an event stream are all per-visitor by
/// definition, so none of them is stored.
#[tokio::test]
async fn cache_e2e_the_responses_the_origin_refuses_to_share_are_not_stored() {
    let backend = CountingBackend::start().await;
    let ngx = start_gateway(&backend, &cache_config(1)).await;
    let addr = format!("127.0.0.1:{}", ngx.http_port);

    for path in ["/nostore.css", "/private.css", "/cookie.css", "/events.css"] {
        let response = request(&addr, "cache.test", "GET", path, &[]).await;
        assert_eq!(
            header(&response, "X-Cache").as_deref(),
            Some("MISS"),
            "{path} must not be stored: {response}"
        );
        let response = request(&addr, "cache.test", "GET", path, &[]).await;
        assert_eq!(
            header(&response, "X-Cache").as_deref(),
            Some("MISS"),
            "{path} must not be stored: {response}"
        );
    }
    assert_eq!(
        backend.count(),
        8,
        "none of these four responses may be stored"
    );
}

/// A `no-cache` response is stored, but it is stale from the start.
///
/// The second request therefore revalidates it with a conditional
/// request — and the answer still comes from the cache.
#[tokio::test]
async fn cache_e2e_a_no_cache_response_is_stored_but_is_revalidated() {
    let backend = CountingBackend::start().await;
    let ngx = start_gateway(&backend, &cache_config(1)).await;
    let addr = format!("127.0.0.1:{}", ngx.http_port);

    let response = request(&addr, "cache.test", "GET", "/revalidate.css", &[]).await;
    assert_eq!(
        header(&response, "X-Cache").as_deref(),
        Some("MISS"),
        "{response}"
    );

    let response = request(&addr, "cache.test", "GET", "/revalidate.css", &[]).await;
    assert!(response.contains("200 OK"), "{response}");
    assert!(
        response.ends_with("body{color:red}"),
        "the body must come from the cache: {response}"
    );

    let seen = backend.seen().await;
    assert_eq!(seen.len(), 2, "the second request must revalidate");
    assert!(
        seen[1].conditional,
        "the revalidation must carry a conditional request"
    );
}

/// A stale answer is served when the origin fails.
///
/// The response asks for `stale-if-error`, so a failing origin does
/// not take the asset down.
#[tokio::test]
async fn cache_e2e_a_stale_answer_is_served_when_the_origin_fails() {
    let backend = CountingBackend::start().await;
    let ngx = start_gateway(&backend, &cache_config(1)).await;
    let addr = format!("127.0.0.1:{}", ngx.http_port);

    // The first request stores the asset.
    let response = request(&addr, "cache.test", "GET", "/flaky.css", &[]).await;
    assert!(response.contains("200 OK"), "{response}");

    // The origin fails the second one — the backend answers 503 when
    // the request is not the first one for this path.
    let response = request(&addr, "cache.test", "GET", "/flaky.css", &[]).await;
    assert!(
        response.contains("200 OK"),
        "the stale answer must be served: {response}"
    );
    assert!(response.ends_with("body{color:red}"), "{response}");
}

/// A cache that is switched off costs nothing.
///
/// With `cache.enabled: false` the request path never touches the
/// cache: there is no `X-Cache` header and every request reaches the
/// origin.
#[tokio::test]
async fn cache_e2e_a_switched_off_cache_caches_nothing() {
    let backend = CountingBackend::start().await;
    let ngx = start_gateway(&backend, "cache:\n  enabled: false\n").await;
    let addr = format!("127.0.0.1:{}", ngx.http_port);

    let response = request(&addr, "cache.test", "GET", "/a.css", &[]).await;
    assert!(header(&response, "X-Cache").is_none(), "{response}");
    let response = request(&addr, "cache.test", "GET", "/a.css", &[]).await;
    assert!(header(&response, "X-Cache").is_none(), "{response}");
    assert_eq!(backend.count(), 2, "nothing may be cached");
}

/// A cache that cannot store anything still answers the request.
///
/// The cache directory is a path that cannot be created, so the disk
/// tier is off. The requests are answered from the origin, and the
/// client sees no `5xx`.
#[tokio::test]
async fn cache_e2e_a_cache_without_a_directory_still_answers() {
    let backend = CountingBackend::start().await;
    let ngx = start_gateway(
        &backend,
        "cache:\n  enabled: true\n  dir: /proc/pangolin-cache\n  min_uses: 1\n",
    )
    .await;
    let addr = format!("127.0.0.1:{}", ngx.http_port);

    let response = request(&addr, "cache.test", "GET", "/a.css", &[]).await;
    assert!(response.contains("200 OK"), "{response}");
    let response = request(&addr, "cache.test", "GET", "/a.css", &[]).await;
    assert!(response.contains("200 OK"), "{response}");
    assert!(
        !response.contains(" 5"),
        "the client must not see a server error: {response}"
    );
}

/// A request that is not a `GET` is never cached.
///
/// A `HEAD` answers with the headers of the `GET` but no body, and
/// the other methods change the origin's state.
#[tokio::test]
async fn cache_e2e_only_get_is_cached() {
    let backend = CountingBackend::start().await;
    let ngx = start_gateway(&backend, &cache_config(1)).await;
    let addr = format!("127.0.0.1:{}", ngx.http_port);

    let response = request(&addr, "cache.test", "HEAD", "/a.css", &[]).await;
    assert!(response.contains("200 OK"), "{response}");
    let response = request(&addr, "cache.test", "HEAD", "/a.css", &[]).await;
    assert!(response.contains("200 OK"), "{response}");
    assert_eq!(backend.count(), 2, "a HEAD must always reach the origin");
}

/// A `file://` backend is never cached.
///
/// The file is served by the gateway itself, short of the cache: the
/// request never reaches the cache engine at all.
#[tokio::test]
async fn cache_e2e_a_file_backend_is_not_cached() {
    let static_dir = tempfile::Builder::new()
        .prefix("pangolin-e2e-cache-file-")
        .tempdir()
        .expect("tempdir");
    std::fs::write(static_dir.path().join("a.css"), "body{color:red}").expect("write a.css");
    let backend = format!("file://{}", static_dir.path().display());

    let ngx = NgxProcess::start_with_config(
        move |db_path| {
            init_pangolin_db(db_path);
            let conn = Connection::open(db_path).expect("open db");
            seed_site(&conn, "file-site", &backend);
            seed_domain(&conn, "file.test", "file-site");
        },
        &cache_config(1),
    )
    .await;

    let addr = format!("127.0.0.1:{}", ngx.http_port);
    let response = request(&addr, "file.test", "GET", "/a.css", &[]).await;
    assert!(response.contains("200 OK"), "{response}");
    assert!(
        header(&response, "X-Cache").is_none(),
        "a file:// backend is served outside the cache: {response}"
    );
    let response = request(&addr, "file.test", "GET", "/a.css", &[]).await;
    assert!(header(&response, "X-Cache").is_none(), "{response}");
}

/// A tunnel backend is cached like any other.
///
/// The cache sits before the upstream is chosen, so a tunnelled
/// asset is served from the cache the second time round.
#[tokio::test]
async fn cache_e2e_a_tunnel_backend_is_cached() {
    let backend = CountingBackend::start().await;
    let backend_addr = backend.addr.clone();

    let ngx = NgxProcess::start_with_config(
        move |db_path| {
            init_pangolin_db(db_path);
            let conn = Connection::open(db_path).expect("open db");
            seed_tun(&conn, "office");
            seed_site(
                &conn,
                "office-site",
                &format!("office:http://{backend_addr}"),
            );
            seed_domain(&conn, "office.test", "office-site");
        },
        &cache_config(1),
    )
    .await;

    let _tun = TunProcess::start(&ngx, "office", "test-token").await;
    let addr = format!("127.0.0.1:{}", ngx.http_port);

    let response = request(&addr, "office.test", "GET", "/a.css", &[]).await;
    assert!(
        response.contains("200 OK"),
        "{response}\n{}",
        ngx.log_string()
    );
    assert_eq!(
        header(&response, "X-Cache").as_deref(),
        Some("MISS"),
        "{response}"
    );

    let response = request(&addr, "office.test", "GET", "/a.css", &[]).await;
    assert_eq!(
        header(&response, "X-Cache").as_deref(),
        Some("HIT"),
        "{response}"
    );
    assert_eq!(
        backend.count(),
        1,
        "the second request must not reach the origin"
    );
}

/// The access log says whether the cache answered the request.
///
/// The cache's effect has to be visible in the operator's log, not
/// only in the `X-Cache` header the client sees (issue #259).
#[tokio::test]
async fn cache_e2e_the_access_log_says_whether_the_cache_answered() {
    let backend = CountingBackend::start().await;
    let ngx = start_gateway(&backend, &cache_config(1)).await;
    let addr = format!("127.0.0.1:{}", ngx.http_port);

    // Two requests: the first is a miss, the second a hit.
    let _ = request(&addr, "cache.test", "GET", "/a.css", &[]).await;
    let _ = request(&addr, "cache.test", "GET", "/a.css", &[]).await;

    // The admin's live log stream replays the ring buffer, oldest
    // first.
    let cookie = admin_session_cookie(&ngx).await;
    let frames = read_sse_data_frames(
        &format!("127.0.0.1:{}", ngx.admin_port),
        "/api/logs/stream",
        Duration::from_secs(2),
        Some(&cookie),
    )
    .await
    .expect("read the access log stream");

    let answers: Vec<bool> = frames
        .iter()
        .filter_map(|frame| {
            let entry: serde_json::Value = serde_json::from_str(frame).ok()?;
            entry["cache_hit"].as_bool()
        })
        .collect();
    assert_eq!(
        answers,
        vec![false, true],
        "the log must say which request the cache answered: {frames:?}"
    );
}

/// The counters the cache keeps are process-local and never fail a
/// request.
///
/// The test is here because the counters are read by a later issue
/// (the admin statistics card); what it pins is that a request
/// completes with the counters in place.
#[tokio::test]
async fn cache_e2e_the_cache_answers_while_it_keeps_its_counters() {
    let backend = CountingBackend::start().await;
    let ngx = start_gateway(&backend, &cache_config(1)).await;
    let addr = format!("127.0.0.1:{}", ngx.http_port);

    let mut hits = 0usize;
    let mut misses = 0usize;
    for _ in 0..5 {
        let response = request(&addr, "cache.test", "GET", "/a.css", &[]).await;
        match header(&response, "X-Cache").as_deref() {
            Some("HIT") => hits += 1,
            _ => misses += 1,
        }
    }
    assert_eq!(misses, 1);
    assert_eq!(hits, 4);
    assert_eq!(backend.count(), 1);
}
