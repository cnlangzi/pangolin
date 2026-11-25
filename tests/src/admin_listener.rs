//! The admin listener's own failure domain (issue #261).
//!
//! The admin UI is served by a hyper listener of the gateway's own,
//! not by pingora. That is what keeps the two apart: a failure of
//! the admin listener degrades the admin UI and nothing else — the
//! proxy keeps serving the traffic, and the process stays up.

use std::time::Duration;

use crate::harness::{NgxProcess, raw_request, wait_for_log};

/// The proxy keeps serving when the admin port cannot be bound.
///
/// The regression this pins: the admin listener used to be a pingora
/// service, and a bind that failed took the whole process down — the
/// proxy went with it. The listener has a socket of its own now, so a
/// bind that fails is a local failure: the gateway logs it, retries,
/// and serves the traffic all the same.
#[tokio::test]
async fn the_proxy_keeps_serving_when_the_admin_port_cannot_be_bound() {
    let ngx = NgxProcess::start_with_admin_port_blocked(|_| {}).await;

    // The gateway says the admin UI is unavailable.
    assert!(
        wait_for_log(&ngx.log, "cannot listen on", Duration::from_secs(30)).await,
        "the gateway must report that the admin listener cannot bind:\n{}",
        ngx.log_string()
    );

    // The proxy still answers. The ACME challenge path is the one the
    // gateway answers without a backend — it serves it from the
    // certificate directory.
    let token = "admin-listener";
    let dir = ngx.cert_dir().join(".well-known").join("acme-challenge");
    std::fs::create_dir_all(&dir).expect("create the challenge dir");
    std::fs::write(dir.join(token), token.as_bytes()).expect("write the challenge file");

    let addr = format!("127.0.0.1:{}", ngx.http_port);
    let path = format!("/.well-known/acme-challenge/{token}");
    let (status, body) = raw_request(&addr, "probe.example.com", "GET", &path, b"").await;
    assert_eq!(
        status,
        200,
        "the proxy must keep serving: {body}\nngx log:\n{}",
        ngx.log_string()
    );
}

/// The admin UI answers on its own listener.
///
/// The regression this pins: the admin UI moved off pingora (issue
/// #261), and every admin route — the pages, the static assets, the
/// JSON endpoints — has to keep answering on the new listener.
#[tokio::test]
async fn the_admin_ui_answers_on_its_own_listener() {
    let ngx = NgxProcess::start(|_| {}).await;

    let (status, body) = raw_request(
        &format!("127.0.0.1:{}", ngx.admin_port),
        "127.0.0.1",
        "GET",
        "/health",
        b"",
    )
    .await;
    assert_eq!(
        status,
        200,
        "the admin health endpoint must answer: {body}\nngx log:\n{}",
        ngx.log_string()
    );
}
