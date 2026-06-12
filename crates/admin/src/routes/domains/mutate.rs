//! Domains POST / DELETE handlers.
//!
//! Behaviour is preserved byte-for-byte from the pre-refactor
//! `routes/domains.rs` — only file layout changed.

use askama::Template;
use std::sync::Arc;

use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use crate::templates::DomainsNewTemplate;
use crate::{redirect_response, App};

type Resp = Response<Full<Bytes>>;

fn ok_html(body: String) -> http::Result<Resp> {
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .expect("response builder for 200 OK should not fail"))
}

fn parse_form(body: &[u8]) -> std::collections::HashMap<String, String> {
    let body_str = std::str::from_utf8(body).unwrap_or("");
    let mut params = std::collections::HashMap::new();
    for pair in body_str.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            let k = k.trim().to_string();
            let v = urlencoding::decode(v).unwrap_or_default().to_string();
            params.insert(k, v);
        }
    }
    params
}

pub async fn handle_create(app: &Arc<App>, body: &[u8], csrf: &str) -> http::Result<Resp> {
    let params = parse_form(body);
    let domain = params.get("domain").cloned().unwrap_or_default();
    let site_name = params.get("site_name").cloned().unwrap_or_default();
    let auto_issue = params.get("auto_issue").map(|_| true).unwrap_or(false);
    let dns_provider = params
        .get("dns_provider")
        .cloned()
        .unwrap_or_default()
        .trim()
        .to_string();
    let dns_provider = if dns_provider.is_empty() {
        None
    } else {
        Some(dns_provider)
    };

    if domain.is_empty() {
        return render_create_page_with_error(app, "Domain name is required", csrf, None).await;
    }
    if site_name.is_empty() {
        return render_create_page_with_error(app, "Please select a site", csrf, None).await;
    }
    if !pangolin_core::is_valid_domain(&domain) {
        return render_create_page_with_error(
            app,
            "Invalid domain format (use example.com, no scheme)",
            csrf,
            None,
        )
        .await;
    }
    // Wildcard domains must have a DNS association (DNS-01 is the only
    // way to validate `*.example.com`).
    if domain.starts_with("*.") && dns_provider.is_none() {
        return render_create_page_with_error(
            app,
            "Wildcard domains require a DNS provider for DNS-01 validation. \
             Add one under DNS first, then assign it to this domain.",
            csrf,
            None,
        )
        .await;
    }
    // If a DNS provider is referenced, verify it exists.
    if let Some(ref name) = dns_provider {
        let db = app.db.lock().await;
        let exists = pangolin_core::db::get_dns_provider(&db, name)
            .unwrap_or(None)
            .is_some();
        drop(db);
        if !exists {
            return render_create_page_with_error(
                app,
                &format!("DNS provider '{name}' does not exist; create it under DNS first"),
                csrf,
                None,
            )
            .await;
        }
    }

    let d = pangolin_core::types::Domain {
        domain,
        site_name,
        enabled: true,
        auto_issue,
        dns_provider,
        created_at: chrono::Utc::now(),
    };

    let db = app.db.lock().await;
    let result = pangolin_core::db::upsert_domain(&db, &d);
    // Phase-1 lifecycle write (issue #45): an auto-issue domain gets a
    // Pending placeholder row in `certs` immediately, so the dashboard
    // and `/certs` table reflect the operator's intent before the
    // background ACME loop has had a chance to tick. The helper is
    // idempotent — pre-existing rows (Issued, Failed, …) are preserved
    // so the operator's history isn't clobbered by a re-save of the
    // domain form.
    if matches!(result, Ok(())) && d.auto_issue {
        let _ =
            pangolin_core::db::ensure_pending_cert_row(&db, &d.domain, &app.cert_manager.cert_dir);
    }
    drop(db);

    match result {
        Ok(()) => {
            app.reload_indexes().await;
            Ok(redirect_response("/domains"))
        }
        Err(e) => {
            render_create_page_with_error(app, &format!("Database error: {}", e), csrf, None).await
        }
    }
}

/// Re-render the New-domain form with an inline error. Uses the new
/// `pages/domains/new.html` template via `DomainsNewTemplate` (rather
/// than the legacy single-struct form). Behaviour matches the
/// pre-refactor: re-fetches sites and DNS providers so the dropdowns
/// are populated, and applies the inline error to the form.
async fn render_create_page_with_error(
    app: &Arc<App>,
    error: &str,
    csrf: &str,
    preselected_site: Option<&str>,
) -> http::Result<Resp> {
    let db = app.db.lock().await;
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    let dns_providers = pangolin_core::db::list_dns_providers(&db).unwrap_or_default();
    drop(db);
    let html = DomainsNewTemplate {
        sites,
        dns_providers,
        error: Some(error),
        active_nav: "domains",
        preselected_site: preselected_site.map(String::from),
        preselected_site_name: preselected_site.map(String::from),
        dns_provider_value: String::new(),
        auto_issue_checked: false,
        edit_domain: None,
        current_auto_issue: false,
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

pub async fn handle_delete(
    app: &Arc<App>,
    domain: Option<String>,
    _csrf: &str,
) -> http::Result<Resp> {
    if let Some(d) = domain {
        if !d.is_empty() {
            let db = app.db.lock().await;
            let _ = pangolin_core::db::delete_domain(&db, &d);
            drop(db);
            app.reload_indexes().await;
        }
    }
    Ok(redirect_response("/domains"))
}

/// HTMX DELETE /api/domains/{domain} — returns an empty 200 body so HTMX
/// (with hx-swap="delete") can drop the row.
pub async fn api_handle_delete(app: &Arc<App>, domain: String, _csrf: &str) -> http::Result<Resp> {
    if domain.is_empty() {
        return Ok(crate::not_found());
    }
    let db = app.db.lock().await;
    let _ = pangolin_core::db::delete_domain(&db, &domain);
    drop(db);
    app.reload_indexes().await;
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::new()))
        .unwrap())
}
