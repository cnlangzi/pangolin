//! Domains full-page renders.

use askama::Template;
use std::sync::Arc;

use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use crate::templates::{DomainsListTemplate, DomainsNewTemplate, SiteDomainsTemplate};
use crate::App;

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
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}
