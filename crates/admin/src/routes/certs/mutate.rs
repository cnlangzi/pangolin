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
    };

    let db = app.db.lock().await;
    let result = pangolin_core::db::upsert_cert(&db, &c);
    drop(db);

    match result {
        Ok(()) => Ok(redirect_response("/certs")),
        Err(e) => render_create_page_with_error(&format!("Database error: {}", e), csrf),
    }
}

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
