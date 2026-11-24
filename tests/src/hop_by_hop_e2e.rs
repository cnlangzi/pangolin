//! Hop-by-hop header handling — e2e against the real gateway.
//!
//! pingora 0.9 (issue #189) removed two kinds of header from the
//! **upstream** request before it is written to the backend:
//!
//! - the standard hop-by-hop headers (`Connection`, `Keep-Alive`,
//!   `Transfer-Encoding`, `Upgrade`, `TE`, `Trailer`, the
//!   `Proxy-*` family), and
//! - the headers the request's own `Connection` header **nominates**
//!   (`Connection: X-Probe` nominates `X-Probe`), which RFC 9110
//!   §7.6.1 requires to be removed before forwarding.
//!
//! The second half is new for this repository. Its own
//! `strip_hop_by_hop_headers` (`crates/pangolin-core/src/proxy.rs`)
//! never covered the nominated headers, and the direct proxy path
//! deliberately does **not** strip anything itself — it relies on
//! pingora for hop-by-hop handling (see `rewrite_upstream_headers`
//! in `crates/ngx/src/proxy.rs`).
//!
//! Before pingora 0.9 the nomination was not honoured at all: the
//! backend received both `Connection: X-Probe` and `X-Probe: 1`.
//! This test is the regression guard for that: it fails against a
//! build that forwards the nominated header, and it also pins that
//! the gateway's **own** headers still reach the backend — a proxy
//! that strips everything would pass a naive "the header is gone"
//! assertion.
//!
//! The test drives the real `pangolin-ngx` binary (through
//! `harness::NgxProcess`), because the behaviour under test lives in
//! pingora's proxy layer, not in this repository's code.

use chrono::Utc;
use pangolin_core::db;
use pangolin_core::types::{Domain, HostMode, Site};
use rusqlite::Connection;

use crate::harness::{InspectingBackend, NgxProcess, init_pangolin_db, raw_request_with_headers};

// ---------------------------------------------------------------------------
// DB seed helpers
// ---------------------------------------------------------------------------

fn seed_site(conn: &Connection, name: &str, backend: &str, host_mode: HostMode) {
    let now = Utc::now();
    db::upsert_site(
        conn,
        &Site {
            name: name.into(),
            backend: backend.into(),
            enabled: true,
            host_mode,
            host_custom: None,
            created_at: now,
            updated_at: now,
            domain_count: 0,
        },
    )
    .expect("insert site");
}

fn seed_domain(conn: &Connection, domain: &str, site_name: &str) {
    let now = Utc::now();
    db::upsert_domain(
        conn,
        &Domain {
            domain: domain.into(),
            site_name: site_name.into(),
            enabled: true,
            auto_issue: false,
            dns_provider: None,
            challenge_kind: None,
            created_at: now,
        },
    )
    .expect("insert domain");
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

/// A header the request's own `Connection` header nominates must not
/// reach the backend — and neither must the `Connection` header
/// itself.
///
/// RFC 9110 §7.6.1: "A proxy or gateway MUST remove a
/// connection-specific header field before forwarding the message,
/// unless the field is listed in the Connection header field's
/// connection-options." The nominated field (`X-Probe`) is
/// connection-specific by definition, and so is `Connection` itself.
///
/// The second half of the assertion is what keeps the test honest:
/// a gateway that dropped the whole request, or that stripped every
/// header, would also make `X-Probe` disappear. So the test also
/// pins the two headers the gateway is *supposed* to add — the
/// `X-Forwarded-For` hop path and the `Host` rewrite for
/// `host_mode = Backend`.
///
/// The `close` connection option rides along with the nomination
/// (`Connection: close, X-Probe`): the shared request helper reads
/// the answer to EOF, which only happens when the client asks for
/// the connection to be closed. `close` is a connection *option*, not
/// a nominated field, so it changes nothing about what the test
/// asserts.
#[tokio::test]
async fn connection_nominated_headers_are_not_forwarded() {
    let backend = InspectingBackend::start().await;

    let ngx = NgxProcess::start(|db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).unwrap();
        seed_site(
            &conn,
            "hop",
            &format!("http://{}", backend.addr()),
            HostMode::Backend,
        );
        seed_domain(&conn, "hop.test", "hop");
    })
    .await;

    let (status, _body) = raw_request_with_headers(
        &format!("127.0.0.1:{}", ngx.http_port),
        "hop.test",
        "GET",
        "/probe",
        b"",
        &[("Connection", "close, X-Probe"), ("X-Probe", "1")],
    )
    .await;
    assert_eq!(status, 200, "the proxied request must succeed");

    let requests = backend.seen().await;
    assert_eq!(
        requests.len(),
        1,
        "the backend must have received exactly one request"
    );
    let received = &requests[0];

    assert_eq!(received.method, "GET");
    assert_eq!(received.path, "/probe");

    // The nominated header must not have been forwarded …
    assert!(
        !received.has_header("x-probe"),
        "the header nominated by `Connection: X-Probe` must not reach the \
         backend (RFC 9110 §7.6.1); the backend received: {:?}",
        received.headers
    );
    // … and neither must the `Connection` header itself.
    assert!(
        !received.has_header("connection"),
        "`Connection` is a hop-by-hop header and must not reach the \
         backend; the backend received: {:?}",
        received.headers
    );

    // The gateway's own headers still have to be there — otherwise
    // the two assertions above would pass for the wrong reason.
    assert!(
        received.has_header("x-forwarded-for"),
        "the gateway must still add `X-Forwarded-For`; the backend received: {:?}",
        received.headers
    );
    assert_eq!(
        received.header("host"),
        Some(backend.addr()),
        "the gateway must still rewrite `Host` to the backend's authority \
         (host_mode = Backend); the backend received: {:?}",
        received.headers
    );
}
