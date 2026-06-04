//! Domains route — GET /admin/domains.

use std::sync::Arc;
use askama::Template;

use http::Response;
use http_body_util::Full;
use bytes::Bytes;

use crate::App;

fn ok_html(body: String) -> http::Result<Response<Full<Bytes>>> {
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .unwrap())
}

pub async fn render(app: &Arc<App>, csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    let db = app.db.lock().await;
    let domains = pangolin_core::db::list_domains(&db).unwrap_or_default();
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    drop(db);
    ok_html(crate::render_with_assets_and_csrf(crate::templates::DomainsTemplate { domains, sites, active_nav: "domains" }.render().unwrap(), csrf))
}

pub async fn render_table(app: &Arc<App>, _csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    let db = app.db.lock().await;
    let domains = pangolin_core::db::list_domains(&db).unwrap_or_default();
    drop(db);
    let rows: Vec<String> = domains.iter().map(|d| {
        format!(r##"<tr id="domain-{}" class="border-b border-slate-100 hover:bg-slate-50 transition-colors">
  <td class="py-3 px-3"><span class="font-mono text-sm text-slate-800">{}</span></td>
  <td class="py-3 px-3"><span class="text-sm text-slate-600">{}</span></td>
  <td class="py-3 px-3">
    <span class="inline-flex items-center gap-1 text-xs font-medium px-2 py-0.5 rounded-full {}
      {}</span>
  </td>
  <td class="py-3 px-3">
    <div class="flex items-center gap-1">
      <button hx-delete="/admin/api/domains"
        hx-vals='{{"domain": "{}"}}'
        hx-confirm="Delete domain {}?"
        hx-swap="delete"
        class="text-slate-400 hover:text-red-500 p-1 rounded hover:bg-red-50 transition-colors"
        title="Delete">
        <svg class="w-4 h-4" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" d="M14.74 9l-.346 9m-4.788 0L9.26 9m9.968-3.21c.342.052.682.107 1.022.166m-1.022-.165L18.16 19.673a2.25 2.25 0 01-2.244 2.077H8.084a2.25 2.25 0 01-2.244-2.077L4.772 5.79m14.456 0a48.108 48.108 0 00-3.478-.397m-12 .562c.34-.059.68-.114 1.022-.165m0 0a48.11 48.11 0 013.478-.397m7.5 0v-.916c0-1.18-.91-2.164-2.09-2.201a51.964 51.964 0 00-3.32 0c-1.18.037-2.09 1.022-2.09 2.201v.916m7.5 0a48.667 48.667 0 00-7.5 0"/></svg>
      </button>
    </div>
  </td>
</tr>"##,
            d.domain, d.domain, d.site_name,
            if d.enabled { "bg-green-100 text-green-700" } else { "bg-slate-100 text-slate-500" },
            if d.enabled { "" } else { "line-through" },
            d.domain, d.domain
        )
    }).collect();
    ok_html(rows.join(""))
}

pub async fn handle_create(app: &Arc<App>, body: &[u8], csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    let params = parse_form(body);
    let domain = params.get("domain").cloned().unwrap_or_default();
    let site_name = params.get("site_name").cloned().unwrap_or_default();

    if domain.is_empty() {
        return render_form_with_error(app, "Domain name is required", csrf).await;
    }
    if site_name.is_empty() {
        return render_form_with_error(app, "Please select a site", csrf).await;
    }
    if !pangolin_core::is_valid_domain(&domain) {
        return render_form_with_error(app, "Invalid domain format (use example.com, no scheme)", csrf).await;
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
            ok_html(r##"<div id="toast" class="fixed bottom-4 right-4 z-50"><div class="bg-green-600 text-white px-4 py-2 rounded-lg shadow-lg text-sm">Domain created</div></div>"##.to_string())
        }
        Err(e) => render_form_with_error(app, &format!("Database error: {}", e), csrf).await,
    }
}

async fn render_form_with_error(app: &Arc<App>, error: &str, csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    let db = app.db.lock().await;
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    drop(db);
    let html = crate::templates::DomainFormTemplate { sites, error: Some(error) }
        .render()
        .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

pub async fn handle_delete(app: &Arc<App>, domain: Option<String>, _csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    let domain = match domain {
        Some(d) if !d.is_empty() => d,
        _ => return ok_html(r##"<p class="text-red-500 text-sm">Missing domain</p>"##.to_string()),
    };
    let db = app.db.lock().await;
    let result = pangolin_core::db::delete_domain(&db, &domain);
    drop(db);

    match result {
        Ok(true) => {
            app.reload_indexes().await;
            ok_html(r##"<div id="toast" class="fixed bottom-4 right-4 z-50"><div class="bg-slate-700 text-white px-4 py-2 rounded-lg shadow-lg text-sm">Domain deleted</div></div>"##.to_string())
        }
        Ok(false) => ok_html(r##"<p class="text-red-500 text-sm">Domain not found</p>"##.to_string()),
        Err(e) => ok_html(format!(r##"<p class="text-red-500 text-sm">Error: {}</p>"##, e)),
    }
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