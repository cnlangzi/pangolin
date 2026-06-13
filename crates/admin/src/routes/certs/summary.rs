//! `GET /api/certs/summary` — JSON aggregate counts for the dashboard
//! Certs card (issue #45).
//!
//! Backs the "🔴 N  🟡 N" badges. Returned shape is the same flat object
//! the issue brief locks in:
//!
//! ```json
//! {"total": 5, "issued": 3, "pending": 1, "issuing": 0, "failed": 1, "skipped": 0}
//! ```
//!
//! Every status appears in the response (zero-valued buckets included)
//! so the client can render the badge without special-casing missing
//! keys.

use std::sync::Arc;

use bytes::Bytes;
use http::Response;
use http_body_util::Full;
use pangolin_core::CertStatus;

use crate::App;

type Resp = Response<Full<Bytes>>;

pub async fn handle_summary(app: &Arc<App>) -> http::Result<Resp> {
    let db = app.db.lock().await;
    let counts = pangolin_core::db::count_certs_by_status(&db).unwrap_or_default();
    drop(db);

    let total: usize = counts.values().sum();
    // Hand-rolled JSON keeps the response body cheap (no serde_json::Value
    // round-trip) and lets us pin the key order to what the dashboard
    // template reads. The order also matches the issue brief.
    let body = format!(
        "{{\"total\":{},\"issued\":{},\"pending\":{},\"issuing\":{},\"failed\":{},\"skipped\":{}}}",
        total,
        counts.get(&CertStatus::Issued).copied().unwrap_or(0),
        counts.get(&CertStatus::Pending).copied().unwrap_or(0),
        counts.get(&CertStatus::Issuing).copied().unwrap_or(0),
        counts.get(&CertStatus::Failed).copied().unwrap_or(0),
        counts.get(&CertStatus::Skipped).copied().unwrap_or(0),
    );
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "application/json; charset=utf-8")
        // Dashboards poll this endpoint; reusing the existing
        // no-cache + CSRF flow would over-engineer it. A `private`
        // cache flag is enough — operator browsers may reuse a cached
        // value for a few hundred ms while opening multiple tabs.
        .header("Cache-Control", "private, max-age=1")
        .body(Full::new(Bytes::from(body)))
        .expect("response builder for 200 OK should not fail"))
}
