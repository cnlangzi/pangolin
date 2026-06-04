//! Certs route — GET /admin/certs.

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
    let certs = pangolin_core::db::list_certs(&db).unwrap_or_default();
    drop(db);
    let now = chrono::Utc::now();
    ok_html(crate::render_with_assets_and_csrf(crate::templates::CertsTemplate { certs, active_nav: "certs", now: &now }.render().unwrap(), csrf))
}

pub async fn render_form_new(csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    let html = crate::templates::CertFormTemplate { error: None }
        .render()
        .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

pub async fn render_table(app: &Arc<App>, _csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    let db = app.db.lock().await;
    let certs = pangolin_core::db::list_certs(&db).unwrap_or_default();
    drop(db);
    let now = chrono::Utc::now();
    let rows: Vec<String> = certs.iter().map(|c| {
        let expires_str = c.expires_at.map(|e| e.format("%Y-%m-%d").to_string()).unwrap_or_else(|| "unknown".to_string());
        let (status_class, status_text) = match c.expires_at {
            Some(e) => {
                let days_left = (e - now).num_days();
                if days_left < 0 {
                    ("bg-red-100 text-red-700", format!("Expired {}d ago", -days_left))
                } else if days_left < 30 {
                    ("bg-amber-100 text-amber-700", format!("Expires in {}d", days_left))
                } else {
                    ("bg-green-100 text-green-700", "Valid".to_string())
                }
            }
            None => ("bg-slate-100 text-slate-500", "unknown".to_string()),
        };
        format!(r##"<tr id="cert-{}" class="border-b border-slate-100 hover:bg-slate-50 transition-colors">
  <td class="py-3 px-3"><span class="font-mono text-sm text-slate-800">{}</span></td>
  <td class="py-3 px-3 text-sm text-slate-500">{}</td>
  <td class="py-3 px-3"><span class="inline-flex items-center gap-1 text-xs font-medium px-2 py-0.5 rounded-full {}">{}</span></td>
  <td class="py-3 px-3">
    <button hx-delete="/admin/api/certs"
      hx-vals='{{"domain": "{}"}}'
      hx-confirm="Delete cert for {}?"
      hx-swap="delete"
      class="text-slate-400 hover:text-red-500 p-1 rounded hover:bg-red-50 transition-colors"
      title="Delete">
      <svg class="w-4 h-4" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" d="M14.74 9l-.346 9m-4.788 0L9.26 9m9.968-3.21c.342.052.682.107 1.022.166m-1.022-.165L18.16 19.673a2.25 2.25 0 01-2.244 2.077H8.084a2.25 2.25 0 01-2.244-2.077L4.772 5.79m14.456 0a48.108 48.108 0 00-3.478-.397m-12 .562c.34-.059.68-.114 1.022-.165m0 0a48.11 48.11 0 013.478-.397m7.5 0v-.916c0-1.18-.91-2.164-2.09-2.201a51.964 51.964 0 00-3.32 0c-1.18.037-2.09 1.022-2.09 2.201v.916m7.5 0a48.667 48.667 0 00-7.5 0"/></svg>
    </button>
  </td>
</tr>"##,
            c.domain, c.domain, expires_str, status_class, status_text, c.domain, c.domain
        )
    }).collect();
    ok_html(rows.join(""))
}

pub async fn handle_create(app: &Arc<App>, body: &[u8], csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    let params = parse_form(body);
    let domain = params.get("domain").cloned().unwrap_or_default();
    let cert_file = params.get("cert_file").cloned().unwrap_or_default();
    let key_file = params.get("key_file").cloned().unwrap_or_default();
    let expires_at = params.get("expires_at").and_then(|s| {
        if s.is_empty() { None } else {
            chrono::DateTime::parse_from_str(&format!("{}T00:00:00Z", s), "%Y-%m-%dT%H:%M:%SZ").ok().map(|dt| dt.with_timezone(&chrono::Utc))
        }
    });

    if domain.is_empty() {
        return render_form_with_error("Domain is required", csrf);
    }
    if cert_file.is_empty() {
        return render_form_with_error("Certificate file path is required", csrf);
    }
    if key_file.is_empty() {
        return render_form_with_error("Key file path is required", csrf);
    }

    let c = pangolin_core::types::Cert {
        domain,
        cert_file,
        key_file,
        expires_at,
        created_at: chrono::Utc::now(),
    };

    let db = app.db.lock().await;
    let result = pangolin_core::db::upsert_cert(&db, &c);
    drop(db);

    match result {
        Ok(()) => {
            ok_html(r##"<div id="toast" class="fixed bottom-4 right-4 z-50"><div class="bg-green-600 text-white px-4 py-2 rounded-lg shadow-lg text-sm">Cert added</div></div>"##.to_string())
        }
        Err(e) => render_form_with_error(&format!("Database error: {}", e), csrf),
    }
}

fn render_form_with_error(error: &str, csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    let html = crate::templates::CertFormTemplate { error: Some(error) }
        .render()
        .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
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