//! The outbound dashboard channel — the end-to-end suite (issue #262).
//!
//! The node dials **out** to the dashboard: it opens an SSE
//! subscription, executes the commands that arrive on it and reports
//! every result back over an HTTP POST. This suite drives a **real
//! outbound client** against a **mock dashboard**
//! ([`crate::mock_dashboard`]) — the whole path: the handshake, the
//! SSE frames, the router and the report.
//!
//! The dashboard's Ed25519 signature is verified by the **mock**
//! verifier (issue #262, R9): the suite never touches a real key
//! pair, and the production key stays out of the test run.
//!
//! The dashboard is unreachable in one test — a real gateway is
//! started with a dash API that points at a closed port, and the
//! gateway has to keep proxying (issue #262, R16).

use std::sync::Arc;
use std::time::Duration;

use crate::mock_dashboard::{Handshake, MockDashboard};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use ngx::dashboard_client::DashboardClient;
use pangolin_core::{App, CertManager};
use tokio_util::sync::CancellationToken;

/// The token the node and the dashboard share.
const TOKEN: &str = "the-e2e-token";

/// How long a test waits for the node to answer.
const REPORT_TIMEOUT: Duration = Duration::from_secs(15);

/// A running node: the gateway's state and the outbound client.
struct Node {
    app: Arc<App>,
    shutdown: CancellationToken,
    handle: tokio::task::JoinHandle<anyhow::Result<()>>,
    _dir: tempfile::TempDir,
}

impl Drop for Node {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.handle.abort();
    }
}

/// Start a node that dials `dash_api` with `token`.
async fn start_node(dash_api: &str, token: &str) -> Node {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("pangolin.db");
    // The log directories point into the tempdir — the default ones
    // are relative to the working directory, and a test must not write
    // into the repository.
    let mut config = pangolin_core::Config::default();
    config.log.bot.dir = dir.path().join("logs/bots");
    config.log.bot.cache_dir = dir.path().join("logs/bots/cache");
    config.log.referer.dir = dir.path().join("logs/referers");
    let app = Arc::new(
        App::new(
            db_path.to_str().expect("utf8"),
            config,
            CertManager::default(),
        )
        .expect("App::new"),
    );
    {
        let db = app.db.lock().await;
        pangolin_core::db::upsert_api_config(
            &db,
            &pangolin_core::db::ApiConfigRow {
                id: 1,
                instance_id: "e2e-instance".into(),
                token: token.into(),
                dash_api: dash_api.into(),
                updated_at: chrono::Utc::now().to_rfc3339(),
            },
        )
        .expect("seed the api_config row");
    }

    let client = DashboardClient::new(app.clone(), api::protocol::signature::dashboard_key())
        .expect("the outbound client");
    let shutdown = CancellationToken::new();
    let handle = {
        let shutdown = shutdown.clone();
        tokio::spawn(async move { client.run(shutdown).await })
    };

    Node {
        app,
        shutdown,
        handle,
        _dir: dir,
    }
}

/// Decode the body of a report.
fn decode_body(report: &serde_json::Value) -> Vec<u8> {
    let encoded = report["body"].as_str().expect("the body is base64");
    BASE64.decode(encoded).expect("the body is base64")
}

/// The SSE parser reads a command frame out of one chunk.
#[test]
fn the_sse_parser_reads_a_command_frame() {
    let mut parser = ngx::sse_client::SseParser::new();
    let frames = parser.push(b"data: {\"id\":\"c-1\"}\n\n");
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert_eq!(frames[0].data, "{\"id\":\"c-1\"}");
}

/// A frame that is not a command is not executed.
///
/// The dashboard's own control traffic must not be run as an API
/// request — the node only executes the frames it is told to.
#[tokio::test]
async fn a_frame_that_is_not_a_command_is_not_executed() {
    let dashboard = MockDashboard::start(TOKEN).await;
    let _node = start_node(dashboard.base_url(), TOKEN).await;

    dashboard
        .send_frame(
            "keepalive",
            "{\"id\":\"c-12\",\"method\":\"GET\",\"path\":\"/api/v1/sites\"}",
        )
        .await;
    assert!(
        dashboard
            .wait_for_report(Duration::from_secs(3))
            .await
            .is_none(),
        "a frame that is not a command must not be executed"
    );
}

/// The node does not dial a dashboard whose address would put the
/// token on the wire in clear text.
///
/// The channel carries the bearer token in every request, so a
/// plain-HTTP address that leaves the machine is refused — by the
/// settings page and, again, by the client before it dials.
#[tokio::test]
async fn the_node_refuses_to_dial_an_insecure_dashboard_address() {
    let dashboard = MockDashboard::start(TOKEN).await;
    // A plain-HTTP address on a host that is not the loopback: the
    // token would travel in clear text.
    let insecure = dashboard.base_url().replace("127.0.0.1", "example.com");
    let _node = start_node(&insecure, TOKEN).await;

    dashboard.send_get("c-10", "/api/v1/sites").await;
    assert!(
        dashboard
            .wait_for_report(Duration::from_secs(3))
            .await
            .is_none(),
        "the node must not dial an insecure address"
    );
    assert_eq!(dashboard.joins(), 0, "the dashboard must not be called");
}

/// An answer that does not fit in a report is reported as an error.
///
/// The raw log of a busy day is bigger than a report may carry. The
/// dashboard has to learn that the command failed — not wait for an
/// answer that never comes.
#[tokio::test]
async fn an_answer_that_does_not_fit_is_reported_as_an_error() {
    let dashboard = MockDashboard::start(TOKEN).await;
    let node = start_node(dashboard.base_url(), TOKEN).await;

    // A bot log bigger than a report may carry.
    let dir = node.app.config.log.bot.dir.clone();
    std::fs::create_dir_all(&dir).expect("mkdir the bot log dir");
    let date = chrono::Utc::now().date_naive();
    let path = pangolin_core::bot_log::file_path_for(&dir, date);
    std::fs::write(&path, vec![b'x'; 2 * 1024 * 1024]).expect("write the log file");

    let path = format!("/api/v1/logs/bot/raw?date={}", date.format("%Y-%m-%d"));
    dashboard.send_get("c-11", &path).await;
    let report = dashboard
        .wait_for_report(REPORT_TIMEOUT)
        .await
        .expect("the node reports the result");

    assert_eq!(report["id"], "c-11");
    assert_eq!(report["status"], 413, "{report}");
    let body = decode_body(&report);
    let json: serde_json::Value = serde_json::from_slice(&body).expect("the body is json");
    assert_eq!(json["error"]["code"], "result_too_large");
}

/// The node subscribes and executes the command the dashboard sends.
#[tokio::test]
async fn the_node_executes_a_command_the_dashboard_sends() {
    let dashboard = MockDashboard::start(TOKEN).await;
    let _node = start_node(dashboard.base_url(), TOKEN).await;

    dashboard.send_get("c-1", "/api/v1/sites").await;
    let report = dashboard
        .wait_for_report(REPORT_TIMEOUT)
        .await
        .expect("the node reports the result");

    assert_eq!(report["id"], "c-1");
    assert_eq!(report["status"], 200);
    let body = decode_body(&report);
    let json: serde_json::Value = serde_json::from_slice(&body).expect("the body is json");
    assert_eq!(json, serde_json::json!([]));
}

/// A command with a query string answers with the page it asked for.
///
/// The paging parameters are part of the request target, and the
/// target is what the node signs — so a paged read travels down the
/// channel without a change of the protocol.
#[tokio::test]
async fn a_paged_command_answers_with_the_page_it_asked_for() {
    let dashboard = MockDashboard::start(TOKEN).await;
    let node = start_node(dashboard.base_url(), TOKEN).await;

    // Two sites, so a page of one is not the whole list.
    {
        let db = node.app.db.lock().await;
        let now = chrono::Utc::now().to_rfc3339();
        for name in ["first", "second"] {
            db.execute(
                "INSERT INTO sites (name, backend, enabled, host_mode, host_custom, created_at, updated_at)
                 VALUES (?1, 'http://127.0.0.1:9999', 1, 'passthrough', NULL, ?2, ?2)",
                rusqlite::params![name, now],
            )
            .expect("insert the site");
        }
    }

    dashboard
        .send_get("c-13", "/api/v1/sites?limit=1&offset=1")
        .await;
    let report = dashboard
        .wait_for_report(REPORT_TIMEOUT)
        .await
        .expect("the node reports the result");

    assert_eq!(report["id"], "c-13");
    assert_eq!(report["status"], 200, "{report}");
    let body = decode_body(&report);
    let json: serde_json::Value = serde_json::from_slice(&body).expect("the body is json");
    assert_eq!(json.as_array().map(|a| a.len()), Some(1), "{json}");
    assert_eq!(json[0]["name"], "second", "{json}");
}

/// A write command is executed and its result reported.
#[tokio::test]
async fn the_node_executes_a_write_command() {
    let dashboard = MockDashboard::start(TOKEN).await;
    let node = start_node(dashboard.base_url(), TOKEN).await;

    dashboard
        .send_command(serde_json::json!({
            "id": "c-2",
            "method": "POST",
            "path": "/api/v1/sites",
            "body": { "name": "e2e-site", "backend": "http://127.0.0.1:9999" },
        }))
        .await;
    let report = dashboard
        .wait_for_report(REPORT_TIMEOUT)
        .await
        .expect("the node reports the result");

    assert_eq!(report["id"], "c-2");
    assert_eq!(report["status"], 201);
    let body = decode_body(&report);
    let json: serde_json::Value = serde_json::from_slice(&body).expect("the body is json");
    assert_eq!(json["name"], "e2e-site");

    // The site is really in the database — the command ran against
    // the node's own state.
    let db = node.app.db.lock().await;
    let sites = pangolin_core::db::list_sites(&db).expect("the sites");
    assert!(
        sites.iter().any(|s| s.name == "e2e-site"),
        "the site has to be in the database"
    );
}

/// A command for a resource that does not exist is answered with the
/// error envelope — and the node reports it like any other result.
#[tokio::test]
async fn the_node_reports_the_error_of_a_failing_command() {
    let dashboard = MockDashboard::start(TOKEN).await;
    let _node = start_node(dashboard.base_url(), TOKEN).await;

    dashboard.send_get("c-3", "/api/v1/sites/nope").await;
    let report = dashboard
        .wait_for_report(REPORT_TIMEOUT)
        .await
        .expect("the node reports the result");

    assert_eq!(report["status"], 404);
    let body = decode_body(&report);
    let json: serde_json::Value = serde_json::from_slice(&body).expect("the body is json");
    assert_eq!(json["error"]["code"], "site_not_found");
}

/// The node rejects a join answer that echoes a different nonce.
///
/// The regression this pins: a man in the middle that answers the
/// `join` request with its own nonce must not get a command
/// executed on the node.
#[tokio::test]
async fn the_node_rejects_a_join_response_with_a_wrong_nonce() {
    let dashboard = MockDashboard::start(TOKEN).await;
    dashboard.set_handshake(Handshake::WrongNonce).await;
    let _node = start_node(dashboard.base_url(), TOKEN).await;

    dashboard.send_get("c-4", "/api/v1/sites").await;
    assert!(
        dashboard
            .wait_for_report(Duration::from_secs(3))
            .await
            .is_none(),
        "the node must not execute a command on a handshake it refused"
    );
}

/// The node rejects a join answer whose timestamp is outside the
/// window.
#[tokio::test]
async fn the_node_rejects_a_join_response_outside_the_timestamp_window() {
    let dashboard = MockDashboard::start(TOKEN).await;
    dashboard.set_handshake(Handshake::StaleTimestamp).await;
    let _node = start_node(dashboard.base_url(), TOKEN).await;

    dashboard.send_get("c-5", "/api/v1/sites").await;
    assert!(
        dashboard
            .wait_for_report(Duration::from_secs(3))
            .await
            .is_none(),
        "the node must not execute a command on a handshake it refused"
    );
}

/// The node rejects a join answer whose signature does not verify.
#[tokio::test]
async fn the_node_rejects_a_join_response_with_a_bad_signature() {
    let dashboard = MockDashboard::start(TOKEN).await;
    dashboard.set_handshake(Handshake::BadSignature).await;
    let _node = start_node(dashboard.base_url(), TOKEN).await;

    dashboard.send_get("c-6", "/api/v1/sites").await;
    assert!(
        dashboard
            .wait_for_report(Duration::from_secs(3))
            .await
            .is_none(),
        "the node must not execute a command on a handshake it refused"
    );
}

/// The node does not dial the dashboard without a token.
///
/// A gateway that was never configured accepts nothing and reaches
/// out to nothing — the proxy keeps serving (issue #262, R13).
#[tokio::test]
async fn the_node_does_not_dial_without_a_token() {
    let dashboard = MockDashboard::start(TOKEN).await;
    let _node = start_node(dashboard.base_url(), "").await;

    dashboard.send_get("c-7", "/api/v1/sites").await;
    assert!(
        dashboard
            .wait_for_report(Duration::from_secs(3))
            .await
            .is_none(),
        "a node without a token must not dial"
    );
    assert_eq!(dashboard.joins(), 0, "the dashboard must not be called");
}

/// The node signs its requests with the token: a dashboard that
/// expects another token refuses the join.
#[tokio::test]
async fn the_node_signs_its_requests_with_the_token() {
    let dashboard = MockDashboard::start(TOKEN).await;
    // The node holds a different token: its signature does not verify.
    let _node = start_node(dashboard.base_url(), "another-token").await;

    dashboard.send_get("c-8", "/api/v1/sites").await;
    assert!(
        dashboard
            .wait_for_report(Duration::from_secs(3))
            .await
            .is_none(),
        "a node with the wrong token must not get a command executed"
    );
}

/// The node reconnects after the subscription ends.
#[tokio::test]
async fn the_node_reconnects_after_the_subscription_ends() {
    let dashboard = MockDashboard::start(TOKEN).await;
    let _node = start_node(dashboard.base_url(), TOKEN).await;

    // Wait for the first subscription.
    let joined = wait_for(|| dashboard.joins() >= 1, Duration::from_secs(10)).await;
    assert!(joined, "the node has to subscribe at least once");

    dashboard.close_subscriptions().await;

    // The node comes back on its own — within the backoff window.
    let rejoined = wait_for(|| dashboard.joins() >= 2, Duration::from_secs(30)).await;
    assert!(
        rejoined,
        "the node has to rejoin after the subscription ended (joins: {})",
        dashboard.joins()
    );
}

/// A command the dashboard retries is answered with a conflict the
/// second time — and the write of the first attempt is not undone.
///
/// The dashboard's timeout must not roll the node back: the create
/// happened, and the retry says so (issue #262, R17).
#[tokio::test]
async fn a_retried_create_returns_conflict_and_the_dashboard_treats_it_as_success() {
    let dashboard = MockDashboard::start(TOKEN).await;
    let node = start_node(dashboard.base_url(), TOKEN).await;

    let command = serde_json::json!({
        "id": "c-9",
        "method": "POST",
        "path": "/api/v1/sites",
        "body": { "name": "retried-site", "backend": "http://127.0.0.1:9999" },
    });
    dashboard.send_command(command.clone()).await;
    let first = dashboard
        .wait_for_report(REPORT_TIMEOUT)
        .await
        .expect("the node reports the result");
    assert_eq!(first["status"], 201, "{first}");

    // The dashboard did not see the answer and retries the command.
    dashboard.send_command(command).await;
    let second = dashboard
        .wait_for_report(REPORT_TIMEOUT)
        .await
        .expect("the node reports the result");
    assert_eq!(second["status"], 409, "{second}");
    assert_eq!(second["id"], "c-9");

    // The write of the first attempt is still there — the retry did
    // not undo it.
    let db = node.app.db.lock().await;
    let sites = pangolin_core::db::list_sites(&db).expect("the sites");
    assert!(
        sites.iter().any(|s| s.name == "retried-site"),
        "the site has to survive the retry"
    );
}

/// A dashboard that is unreachable does not affect the gateway's
/// forwarding.
///
/// The gateway is the **real binary** here: it has to keep proxying
/// while its dashboard channel is failing (issue #262, R16).
#[tokio::test]
async fn a_dashboard_outage_does_not_affect_forwarding() {
    use crate::harness::{InspectingBackend, NgxProcess, init_pangolin_db, raw_request};

    let backend = InspectingBackend::start().await;
    let backend_addr = backend.addr().to_string();

    let ngx = NgxProcess::start(move |db_path| {
        init_pangolin_db(db_path);
        let conn = rusqlite::Connection::open(db_path).expect("open the database");
        // A dash API that points at a closed port: the channel fails
        // from the first attempt on.
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO api_config (id, instance_id, token, dash_api, updated_at)
             VALUES (1, 'e2e-instance', 'the-e2e-token', 'http://127.0.0.1:1/api/v1/', ?1)",
            rusqlite::params![now],
        )
        .expect("seed the api_config row");
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO sites (name, backend, enabled, host_mode, host_custom, created_at, updated_at)
             VALUES ('direct-site', ?1, 1, 'passthrough', NULL, ?2, ?2)",
            rusqlite::params![format!("http://{backend_addr}"), now],
        )
        .expect("insert the site");
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO domains (domain, site_name, enabled, created_at) VALUES ('direct.test', 'direct-site', 1, ?1)",
            rusqlite::params![now],
        )
        .expect("insert the domain");
    })
    .await;

    let addr = format!("127.0.0.1:{}", ngx.http_port);
    let (status, body) = raw_request(&addr, "direct.test", "GET", "/api/echo", b"").await;
    assert_eq!(
        status, 200,
        "the gateway has to keep proxying while the dashboard is down (body={body:?})"
    );

    // The channel really was failing — the node could not reach the
    // dashboard it was told to dial.
    let log = ngx.log_string();
    assert!(
        log.contains("dashboard:"),
        "the dashboard channel has to report its failure:\n{log}"
    );
}

/// The node's outbound client refuses a dashboard whose certificate
/// no root store signs.
///
/// The client's roots are the **system's** certificate store (issue
/// #262, R18): a dashboard with a self-signed certificate is refused,
/// exactly like a provider range list served by one. There is no
/// "skip verification" path.
#[tokio::test]
async fn the_outbound_client_refuses_an_invalid_certificate() {
    use std::convert::Infallible;
    use std::sync::Arc;

    use bytes::Bytes;
    use http_body_util::Full;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use tokio::net::TcpListener;

    // A certificate nobody signed.
    let cert_key = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("generate the self-signed certificate");
    let certs = vec![
        CertificateDer::from_pem_reader(cert_key.cert.pem().as_bytes())
            .expect("the certificate parses"),
    ];
    let key = PrivateKeyDer::from_pem_reader(cert_key.signing_key.serialize_pem().as_bytes())
        .expect("the key parses");
    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .expect("the server config builds");
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the TLS server");
    let port = listener.local_addr().expect("the local address").port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(stream).await else {
                    return;
                };
                let service = service_fn(|_req| async {
                    Ok::<_, Infallible>(hyper::Response::new(Full::new(Bytes::from_static(b"ok"))))
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(tls), service)
                    .await;
            });
        }
    });

    let client = ngx::http_client::build_stream_client().expect("the outbound client builds");
    let error = client
        .get(format!("https://localhost:{port}/"))
        .send()
        .await
        .expect_err("an untrusted certificate has to be refused");
    let message = format!("{error:?}");
    assert!(
        message.to_lowercase().contains("certificate"),
        "the refusal has to name the certificate: {message}"
    );
}

/// Wait until `condition` holds.
async fn wait_for<F: Fn() -> bool>(condition: F, within: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    while tokio::time::Instant::now() < deadline {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    condition()
}
