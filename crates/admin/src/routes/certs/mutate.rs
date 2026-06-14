//! Certs POST / DELETE handlers.

use std::sync::Arc;

use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use crate::{redirect_response, App};

use super::helpers::parse_form;
use super::pages::render_create_page_with_error;

type Resp = Response<Full<Bytes>>;

pub async fn handle_create(app: &Arc<App>, body: &[u8], csrf: &str) -> http::Result<Resp> {
    let params = parse_form(body);
    let domain = params.get("domain").cloned().unwrap_or_default();
    let cert_file = params.get("cert_file").cloned().unwrap_or_default();
    let key_file = params.get("key_file").cloned().unwrap_or_default();
    let expires_at = params.get("expires_at").and_then(|s| {
        if s.is_empty() {
            None
        } else {
            chrono::DateTime::parse_from_str(&format!("{}T00:00:00Z", s), "%Y-%m-%dT%H:%M:%SZ")
                .ok()
                .map(|dt| dt.with_timezone(&chrono::Utc))
        }
    });

    if domain.is_empty() {
        return render_create_page_with_error("Domain is required", csrf);
    }
    if cert_file.is_empty() {
        return render_create_page_with_error("Certificate file path is required", csrf);
    }
    if key_file.is_empty() {
        return render_create_page_with_error("Key file path is required", csrf);
    }

    let c = pangolin_core::types::Cert {
        domain,
        cert_file,
        key_file,
        expires_at,
        created_at: chrono::Utc::now(),
        sans: vec![],
        source: "manual".to_string(),
        acme_dns_provider: None,
        acme_account_id: None,
        issued_at: 0,
        // Manual uploads bypass the ACME flow entirely, so the row goes
        // straight to `Issued` with no `started_at` / `last_error`.
        status: pangolin_core::types::CertStatus::Issued,
        started_at: None,
        last_error: None,
        next_retry_at: None,
        error_class: None,
        attempt_count: 0,
        order_url: None,
    };

    let db = app.db.lock().await;
    let result = pangolin_core::db::upsert_cert(&db, &c);
    drop(db);

    match result {
        Ok(()) => Ok(redirect_response("/certs")),
        Err(e) => render_create_page_with_error(&format!("Database error: {}", e), csrf),
    }
}

// NOTE: `handle_delete` is the legacy form-POST endpoint. It will be removed
// once all admin UIs migrate to the HTMX `hx-delete` button (see
// `api_handle_delete` below and the `templates/components/_hx_delete_button.html`
// partial). Tracked by issue #48.
pub async fn handle_delete(
    app: &Arc<App>,
    domain: Option<String>,
    _csrf: &str,
) -> http::Result<Resp> {
    if let Some(d) = domain {
        if !d.is_empty() {
            let db = app.db.lock().await;
            let _ = pangolin_core::db::delete_cert(&db, &d);
            drop(db);
            app.reload_indexes().await;
        }
    }
    Ok(redirect_response("/certs"))
}

/// HTMX `DELETE /api/certs/{domain}` — returns an empty 200 body so HTMX
/// (with `hx-swap="delete"`) can drop the row without a full page reload.
///
/// This is the unified delete endpoint for certs; the form-POST
/// `/certs/delete` route above is kept for now as a fallback during the
/// migration window (issue #48).
pub async fn api_handle_delete(app: &Arc<App>, domain: String, _csrf: &str) -> http::Result<Resp> {
    if domain.is_empty() {
        return Ok(crate::not_found());
    }
    let db = app.db.lock().await;
    let _ = pangolin_core::db::delete_cert(&db, &domain);
    drop(db);
    app.reload_indexes().await;
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::new()))
        .unwrap())
}

/// `POST /certs/retry` — operator-driven ACME retry (issue #45).
///
/// Looks up the domain row and dispatches through the [`pangolin_core::CertRetrier`]
/// bridge so this handler stays decoupled from `ngx::acme::AcmeState`
/// (admin would otherwise have to depend on ngx + pingora). The
/// retrier itself drives the status row (`Pending`/`Issuing`/…), so
/// this handler just kicks the work off and redirects back.
///
/// We do NOT await the full issuance here — ACME flows can take
/// seconds (DNS-01 propagation, polling for the cert). Instead we
/// spawn the retrier on the host runtime and respond immediately so
/// the operator's browser doesn't hang. The status row is updated
/// asynchronously by the spawned task; the UI converges on the next
/// page load (or the future htmx auto-refresh).
pub async fn handle_retry(app: &Arc<App>, body: &[u8], _csrf: &str) -> http::Result<Resp> {
    let params = parse_form(body);
    let domain = params.get("domain").cloned().unwrap_or_default();
    if domain.is_empty() {
        return Ok(redirect_response("/certs"));
    }

    // Take a clone of the retrier so we can drop the read guard
    // immediately. The spawned task owns the Arc for the duration of
    // the issuance.
    let retrier = app.cert_retrier.read().await.clone();
    match retrier {
        Some(retrier) => {
            let domain_for_task = domain.clone();
            tokio::spawn(async move {
                if let Err(e) = retrier.retry(&domain_for_task).await {
                    log::warn!("cert retry {} failed: {}", domain_for_task, e);
                }
            });
        }
        None => {
            // Process started without an ACME pipeline (e.g. admin-only
            // unit-test harness). Surface this rather than silently
            // succeeding so the operator knows the click did nothing.
            log::warn!(
                "POST /certs/retry but no CertRetrier installed (acme service \
                 not running?); domain={}",
                domain
            );
        }
    }
    Ok(redirect_response("/certs"))
}
