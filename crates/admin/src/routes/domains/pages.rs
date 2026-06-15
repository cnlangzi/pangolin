//! Domains full-page renders.

use askama::Template;
use std::sync::Arc;

use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use crate::App;
use crate::templates::{
    DomainsEditTemplate, DomainsListTemplate, DomainsNewTemplate, SiteDomainsTemplate,
};

type Resp = Response<Full<Bytes>>;

fn ok_html(body: String) -> http::Result<Resp> {
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .unwrap())
}

pub async fn render(app: &Arc<App>, csrf: &str) -> http::Result<Resp> {
    let db = app.db.lock().await;
    let domains = pangolin_core::db::list_domains(&db).unwrap_or_default();
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    drop(db);
    let html = DomainsListTemplate {
        domains,
        sites,
        active_nav: "domains",
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

pub async fn render_create_page(app: &Arc<App>, csrf: &str) -> http::Result<Resp> {
    let db = app.db.lock().await;
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    let dns_providers = pangolin_core::db::list_dns_providers(&db).unwrap_or_default();
    drop(db);
    let html = DomainsNewTemplate {
        sites,
        dns_providers,
        error: None,
        active_nav: "domains",
        preselected_site: None,
        preselected_site_name: None,
        dns_provider_value: String::new(),
        auto_issue_checked: false,
        edit_domain: None,
        current_auto_issue: false,
        enabled_checked: true,
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

/// Render the Edit-domain page (`GET /domains/{domain}/edit`).
///
/// Issue #57: pre-fills the `DomainsEditTemplate` with the row's
/// current values for all 4 editable fields (site_name, enabled,
/// auto_issue, dns_provider). The PK (`domain`) is rendered read-only
/// in the form via `lock_domain()`; the site dropdown is also locked
/// (we still send the value in a hidden field). Returns 404 if the
/// row doesn't exist.
pub async fn render_edit_page(
    app: &Arc<App>,
    domain: Option<String>,
    csrf: &str,
) -> http::Result<Resp> {
    let Some(domain_pk) = domain else {
        return Ok(crate::not_found());
    };
    if domain_pk.is_empty() {
        return Ok(crate::not_found());
    }
    let db = app.db.lock().await;
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    let dns_providers = pangolin_core::db::list_dns_providers(&db).unwrap_or_default();
    let existing = pangolin_core::db::get_domain(&db, &domain_pk).unwrap_or(None);
    drop(db);

    let Some(existing) = existing else {
        return Ok(crate::not_found());
    };

    let html = DomainsEditTemplate {
        sites,
        dns_providers,
        error: None,
        active_nav: "domains",
        preselected_site: Some(existing.site_name.clone()),
        preselected_site_name: Some(existing.site_name.clone()),
        dns_provider_value: existing.dns_provider.clone().unwrap_or_default(),
        auto_issue_checked: existing.auto_issue,
        edit_domain: Some(existing.domain.clone()),
        current_auto_issue: existing.auto_issue,
        enabled_checked: existing.enabled,
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

/// Render the site-specific domains sub-page (`/site/{name}/domains`).
/// This shows domains mapped to a single site with a per-row delete
/// button.
pub async fn render_for_site(app: &Arc<App>, site_name: &str, csrf: &str) -> http::Result<Resp> {
    let db = app.db.lock().await;
    let all_domains = pangolin_core::db::list_domains(&db).unwrap_or_default();
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    let site = sites.iter().find(|s| s.name == site_name).cloned();
    drop(db);

    let site_name_owned = site_name.to_owned();
    let domains: Vec<_> = all_domains
        .into_iter()
        .filter(|d| d.site_name == site_name_owned)
        .collect();

    let site = match site {
        Some(s) => s,
        None => {
            let mut resp = Response::new(Full::new(Bytes::from(
                "<p class='text-red-500 text-sm'>Site not found</p>",
            )));
            *resp.status_mut() = http::StatusCode::NOT_FOUND;
            return Ok(resp);
        }
    };

    let html = SiteDomainsTemplate {
        site,
        domains,
        sites,
        active_nav: "domains",
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

/// Render the New-domain form, with a preselected site, for embedding
/// inside a modal. Used by `/api/site/{name}/domains/new` (HTMX).
pub async fn api_render_form_new(
    app: &Arc<App>,
    site_name: &str,
    _body: &[u8],
    csrf: &str,
) -> http::Result<Resp> {
    use crate::templates::DomainsNewTemplate;
    use askama::Template;

    let db = app.db.lock().await;
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    let dns_providers = pangolin_core::db::list_dns_providers(&db).unwrap_or_default();
    drop(db);
    let html = DomainsNewTemplate {
        sites,
        dns_providers,
        error: None,
        active_nav: "domains",
        preselected_site: Some(site_name.to_string()),
        preselected_site_name: Some(site_name.to_string()),
        dns_provider_value: String::new(),
        auto_issue_checked: false,
        edit_domain: None,
        current_auto_issue: false,
        enabled_checked: true,
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}
