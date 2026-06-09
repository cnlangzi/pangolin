//! Domains route — list / new / delete (full-page, no modal).

use askama::Template;
use std::sync::Arc;

use bytes::Bytes;
use http::{Response, StatusCode};
use http_body_util::Full;

use crate::templates::DomainFormTemplate;
use crate::{redirect_response, App};

fn ok_html(body: String) -> http::Result<Response<Full<Bytes>>> {
    let resp = Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .expect("response builder for 200 OK should not fail");
    Ok(resp)
}

pub async fn render(app: &Arc<App>, csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    let db = app.db.lock().await;
    let domains = pangolin_core::db::list_domains(&db).unwrap_or_default();
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    drop(db);
    ok_html(crate::render_with_assets_and_csrf(
        crate::templates::DomainsTemplate {
            domains,
            sites,
            active_nav: "domains",
        }
        .render()
        .unwrap(),
        csrf,
    ))
}

pub async fn render_create_page(app: &Arc<App>, csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    let db = app.db.lock().await;
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    drop(db);
    let html = DomainFormTemplate {
        sites,
        error: None,
        active_nav: "domains",
        preselected_site: None,
        preselected_site_name: None,
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

/// Render domains filtered by a specific site — used for /admin/site/{name}/domains.
pub async fn render_for_site(
    app: &Arc<App>,
    site_name: &str,
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
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
            *resp.status_mut() = StatusCode::NOT_FOUND;
            return Ok(resp);
        }
    };

    ok_html(crate::render_with_assets_and_csrf(
        crate::templates::SiteDomainsTemplate {
            site,
            domains,
            sites,
        }
        .render()
        .unwrap(),
        csrf,
    ))
}

/// Render only the table rows for site-specific domains (htmx partial).
pub async fn render_table_for_site(
    app: &Arc<App>,
    site_name: &str,
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    let db = app.db.lock().await;
    let all_domains = pangolin_core::db::list_domains(&db).unwrap_or_default();
    drop(db);
    let site_name_owned = site_name.to_owned();
    let domains: Vec<_> = all_domains
        .into_iter()
        .filter(|d| d.site_name == site_name_owned)
        .collect();

    let rows: Vec<String> = domains
        .iter()
        .map(|d| {
            format!(
                r##"<tr id="domain-{}" class="border-b border-slate-100 dark:border-slate-700 hover:bg-slate-50 dark:hover:bg-slate-700/30 transition-colors">
  <td class="py-3 px-3"><span class="font-mono text-sm text-slate-800 dark:text-slate-100">{}</span></td>
  <td class="py-3 px-3">
    <span class="inline-flex items-center gap-1 text-xs font-medium px-2 py-0.5 rounded-full {} {} {}">{}</span>
  </td>
  <td class="py-3 px-3">
    <div class="flex items-center gap-1">
      <form method="POST" action="/admin/domains/delete" onsubmit="return confirm('Delete domain {}?');" class="inline">
        <input type="hidden" name="domain" value="{}">
        <input type="hidden" name="_csrf" value="__CSRF__">
        <button type="submit"
          class="text-slate-400 dark:text-slate-400 hover:text-red-500 dark:hover:text-red-400 p-1.5 rounded hover:bg-red-50 dark:hover:bg-red-500/10 transition-colors"
          title="Delete">
          <svg class="w-4 h-4" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" d="M14.74 9l-.346 9m-4.788 0L9.26 9m9.968-3.21c.342.052.682.107 1.022.166m-1.022-.165L18.16 19.673a2.25 2.25 0 01-2.244 2.077H8.084a2.25 2.25 0 01-2.244-2.077L4.772 5.79m14.456 0a48.108 48.108 0 00-3.478-.397m-12 .562c.34-.059.68-.114 1.022-.165m0 0a48.11 48.11 0 013.478-.397m7.5 0v-.916c0-1.18-.91-2.164-2.09-2.201a51.964 51.964 0 00-3.32 0c-1.18.037-2.09 1.022-2.09 2.201v.916m7.5 0a48.667 48.667 0 00-7.5 0"/></svg>
        </button>
      </form>
    </div>
  </td>
</tr>"##,
                d.domain,
                d.domain,
                if d.enabled {
                    "bg-green-100 text-green-700"
                } else {
                    "bg-slate-100 text-slate-400"
                },
                if d.enabled {
                    "dark:bg-green-900/30 dark:text-green-300"
                } else {
                    "dark:bg-slate-700 dark:text-slate-500"
                },
                if d.enabled { "" } else { "line-through" },
                if d.enabled { "enabled" } else { "disabled" },
                d.domain,
                d.domain
            )
        })
        .collect();

    ok_html(crate::render_with_assets_and_csrf(rows.join(""), csrf))
}

/// Render the "new domain" form pre-seeded with a site name (from site-specific sub-page).
pub async fn api_render_form_new(
    app: &Arc<App>,
    site_name: &str,
    _body: &[u8],
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    let db = app.db.lock().await;
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    drop(db);
    let html = DomainFormTemplate {
        sites,
        error: None,
        active_nav: "domains",
        preselected_site: Some(site_name.to_string()),
        preselected_site_name: Some(site_name.to_string()),
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

pub async fn handle_create(
    app: &Arc<App>,
    body: &[u8],
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    let params = parse_form(body);
    let domain = params.get("domain").cloned().unwrap_or_default();
    let site_name = params.get("site_name").cloned().unwrap_or_default();

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

    let d = pangolin_core::types::Domain {
        domain,
        site_name,
        enabled: true,
        created_at: chrono::Utc::now(),
    };

    let db = app.db.lock().await;
    let result = pangolin_core::db::upsert_domain(&db, &d);
    drop(db);

    match result {
        Ok(()) => {
            app.reload_indexes().await;
            Ok(redirect_response("/admin/domains"))
        }
        Err(e) => {
            render_create_page_with_error(app, &format!("Database error: {}", e), csrf, None).await
        }
    }
}

async fn render_create_page_with_error(
    app: &Arc<App>,
    error: &str,
    csrf: &str,
    preselected_site: Option<&str>,
) -> http::Result<Response<Full<Bytes>>> {
    let db = app.db.lock().await;
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    drop(db);
    let html = DomainFormTemplate {
        sites,
        error: Some(error),
        active_nav: "domains",
        preselected_site: preselected_site.map(String::from),
        preselected_site_name: preselected_site.map(String::from),
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

pub async fn handle_delete(
    app: &Arc<App>,
    domain: Option<String>,
    _csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    if let Some(d) = domain {
        if !d.is_empty() {
            let db = app.db.lock().await;
            let _ = pangolin_core::db::delete_domain(&db, &d);
            drop(db);
            app.reload_indexes().await;
        }
    }
    Ok(redirect_response("/admin/domains"))
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
