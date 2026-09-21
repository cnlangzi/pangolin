//! In-memory traffic page + side-channel — e2e.
//!
//! Pins: `/traffic` requires auth; the page renders; a proxied
//! request shows up in the HTMX KPI fragment. Would have failed
//! before the traffic module existed (404 / empty counters).

use std::time::Duration;

use chrono::Utc;
use pangolin_core::db;
use pangolin_core::types::{Domain, HostMode, Site};
use reqwest::Client;
use rusqlite::Connection;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::timeout;

use crate::admin_harness::AdminClient;
use crate::harness::{NgxProcess, init_pangolin_db, raw_request};

fn seed_site(conn: &Connection, name: &str, backend: &str) {
    let now = Utc::now();
    db::upsert_site(
        conn,
        &Site {
            name: name.into(),
            backend: backend.into(),
            enabled: true,
            host_mode: HostMode::Passthrough,
            host_custom: None,
            created_at: now,
            updated_at: now,
            domain_count: 0,
        },
    )
    .expect("insert site");
}

fn seed_domain(conn: &Connection, domain: &str, site_name: &str) {
    db::upsert_domain(
        conn,
        &Domain {
            domain: domain.into(),
            site_name: site_name.into(),
            enabled: true,
            auto_issue: false,
            dns_provider: None,
            challenge_kind: None,
            created_at: Utc::now(),
        },
    )
    .expect("insert domain");
}

struct MockHttpBackend {
    addr: String,
    _handle: tokio::task::JoinHandle<()>,
}

impl MockHttpBackend {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let handle = tokio::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => break,
                };
                tokio::spawn(async move {
                    let mut buf = Vec::with_capacity(2048);
                    let mut tmp = [0u8; 1024];
                    loop {
                        match timeout(Duration::from_secs(2), stream.read(&mut tmp)).await {
                            Ok(Ok(0)) => break,
                            Ok(Ok(n)) => {
                                buf.extend_from_slice(&tmp[..n]);
                                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Ok(Err(_)) | Err(_) => break,
                        }
                    }
                    let body = b"ok";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                    let _ = stream.write_all(body).await;
                });
            }
        });
        Self {
            addr,
            _handle: handle,
        }
    }
}

#[tokio::test]
async fn traffic_admin_page_requires_auth() {
    let _ngx = NgxProcess::start(init_pangolin_db).await;
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let resp = client
        .get(&format!("http://127.0.0.1:{}/traffic", _ngx.admin_port))
        .send()
        .await
        .expect("GET /traffic");
    assert_eq!(resp.status().as_u16(), 302);
    let loc = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(loc.contains("/login"), "redirect target: {loc}");
}

#[tokio::test]
async fn traffic_admin_page_renders() {
    let _ngx = NgxProcess::start(init_pangolin_db).await;
    let client = AdminClient::new(&_ngx);
    client.login("admin", "admin").await.expect("login");
    let resp = client.get("/traffic").await.expect("GET /traffic");
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.expect("body");
    assert!(body.contains("Traffic"), "{body}");
    assert!(
        body.contains("/api/traffic/kpis"),
        "page must poll KPIs: {body}"
    );
}

#[tokio::test]
async fn traffic_kpis_count_proxied_request() {
    let backend = MockHttpBackend::start().await;
    let ngx = NgxProcess::start(|db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).unwrap();
        seed_site(&conn, "web", &format!("http://{}", backend.addr));
        seed_domain(&conn, "traffic.test", "web");
    })
    .await;

    let (status, _) = raw_request(
        &format!("127.0.0.1:{}", ngx.http_port),
        "traffic.test",
        "GET",
        "/hello",
        b"",
    )
    .await;
    assert_eq!(status, 200, "proxied GET must succeed");

    let client = AdminClient::new(&ngx);
    client.login("admin", "admin").await.expect("login");

    let mut saw = false;
    for _ in 0..20 {
        let resp = client
            .get("/api/traffic/kpis")
            .await
            .expect("GET /api/traffic/kpis");
        assert_eq!(resp.status().as_u16(), 200);
        let body = resp.text().await.expect("body");
        // The KPI card prints requests_total as a raw integer. After
        // one proxied request it must not still be the zero default.
        if body.contains(">1<") || body.contains(">2<") || !body.contains(">0<") {
            // Prefer a stricter check: the Requests card is the
            // second KPI; look for a non-zero total nearby.
            if body.contains("Requests")
                && (body.contains(">1<")
                    || body.contains(">2<")
                    || body.contains(">3<")
                    || body.contains(">4<")
                    || body.contains(">5<"))
            {
                saw = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        saw,
        "KPI fragment never showed a non-zero request count after a proxied GET"
    );
}
