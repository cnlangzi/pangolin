//! Domains full-page renders.

use askama::Template;
use std::sync::Arc;

use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use crate::App;
use crate::i18n::Lang;
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

pub async fn render(app: &Arc<App>, csrf: &str, lang: Lang) -> http::Result<Resp> {
    // The reads run on the blocking pool, under one connection
    // (issue #254).
    let (domains, sites) = app
        .with_db_or(|db| {
            (
                pangolin_core::db::list_domains(db).unwrap_or_default(),
                pangolin_core::db::list_sites(db).unwrap_or_default(),
            )
        })
        .await;
    let html = DomainsListTemplate {
        domains,
        sites,
        active_nav: "domains",
        lang,
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

pub async fn render_create_page(app: &Arc<App>, csrf: &str, lang: Lang) -> http::Result<Resp> {
    // The reads run on the blocking pool, under one connection
    // (issue #254).
    let (sites, dns_providers) = app
        .with_db_or(|db| {
            (
                pangolin_core::db::list_sites(db).unwrap_or_default(),
                pangolin_core::db::list_dns_providers(db).unwrap_or_default(),
            )
        })
        .await;
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
        challenge_kind_value: String::new(),
        lang,
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
    lang: Lang,
) -> http::Result<Resp> {
    let Some(domain_pk) = domain else {
        return Ok(crate::not_found(lang));
    };
    if domain_pk.is_empty() {
        return Ok(crate::not_found(lang));
    }
    // The reads run on the blocking pool, under one connection
    // (issue #254).
    let (sites, dns_providers, existing) = app
        .with_db_or(move |db| {
            (
                pangolin_core::db::list_sites(db).unwrap_or_default(),
                pangolin_core::db::list_dns_providers(db).unwrap_or_default(),
                pangolin_core::db::get_domain(db, &domain_pk).unwrap_or(None),
            )
        })
        .await;

    let Some(existing) = existing else {
        return Ok(crate::not_found(lang));
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
        // Pre-fill the dropdown from the row's stored kind (issue #55)
        // so the operator sees the persisted value, not a hardcoded
        // "Auto". Mirrors `render_edit_page_with_error` in mutate.rs.
        challenge_kind_value: existing
            .challenge_kind
            .map(|k| k.as_str().to_string())
            .unwrap_or_default(),
        lang,
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

/// Render the site-specific domains sub-page (`/site/{name}/domains`).
/// This shows domains mapped to a single site with a per-row delete
/// button.
pub async fn render_for_site(
    app: &Arc<App>,
    site_name: &str,
    csrf: &str,
    lang: Lang,
) -> http::Result<Resp> {
    // The reads run on the blocking pool, under one connection
    // (issue #254).
    let (all_domains, sites) = app
        .with_db_or(|db| {
            (
                pangolin_core::db::list_domains(db).unwrap_or_default(),
                pangolin_core::db::list_sites(db).unwrap_or_default(),
            )
        })
        .await;
    let site = sites.iter().find(|s| s.name == site_name).cloned();

    let site_name_owned = site_name.to_owned();
    let domains: Vec<_> = all_domains
        .into_iter()
        .filter(|d| d.site_name == site_name_owned)
        .collect();

    let site = match site {
        Some(s) => s,
        None => {
            let mut resp = Response::new(Full::new(Bytes::from(format!(
                "<p class='text-red-500 text-sm'>{}</p>",
                lang.t("domains.error.site_not_found")
            ))));
            *resp.status_mut() = http::StatusCode::NOT_FOUND;
            return Ok(resp);
        }
    };

    let html = SiteDomainsTemplate {
        site,
        domains,
        sites,
        active_nav: "domains",
        lang,
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
    lang: Lang,
) -> http::Result<Resp> {
    use crate::templates::DomainsNewTemplate;
    use askama::Template;

    // The reads run on the blocking pool, under one connection
    // (issue #254).
    let (sites, dns_providers) = app
        .with_db_or(|db| {
            (
                pangolin_core::db::list_sites(db).unwrap_or_default(),
                pangolin_core::db::list_dns_providers(db).unwrap_or_default(),
            )
        })
        .await;
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
        challenge_kind_value: String::new(),
        lang,
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}
