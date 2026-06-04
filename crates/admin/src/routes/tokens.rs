//! Tokens route — GET /admin/tokens (+ CRUD).

use std::sync::Arc;
use askama::Template;

use http::Response;
use http_body_util::Full;
use bytes::Bytes;

use crate::App;

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
    let tokens = pangolin_core::db::list_tokens(&db).unwrap_or_default();
    drop(db);
    ok_html(crate::render_with_assets_and_csrf(crate::templates::TokensTemplate { tokens, active_nav: "tokens" }.render().unwrap(), csrf))
}

pub async fn render_table(app: &Arc<App>, _csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    let db = app.db.lock().await;
    let tokens = pangolin_core::db::list_tokens(&db).unwrap_or_default();
    drop(db);
    let now = chrono::Utc::now();
    let rows: Vec<String> = tokens.iter().map(|t| {
        let expires_str = t.expires_at.map(|e| e.format("%Y-%m-%d").to_string()).unwrap_or_else(|| "never".to_string());
        let status_class = if t.enabled {
            if t.expires_at.map(|e| e < now).unwrap_or(false) {
                "bg-red-100 text-red-700"
            } else {
                "bg-green-100 text-green-700"
            }
        } else {
            "bg-slate-100 text-slate-500 line-through"
        };
        let status_text = if t.enabled {
            if t.expires_at.map(|e| e < now).unwrap_or(false) {
                "expired"
            } else {
                "active"
            }
        } else {
            "disabled"
        };
        format!(r##"<tr id="token-{}" class="border-b border-slate-100 hover:bg-slate-50 transition-colors">
  <td class="py-3 px-3"><code class="text-sm text-slate-700 bg-slate-100 px-2 py-0.5 rounded">{}</code></td>
  <td class="py-3 px-3"><span class="inline-flex items-center gap-1 text-xs font-medium px-2 py-0.5 rounded-full {}">{}</span></td>
  <td class="py-3 px-3 text-sm text-slate-500">{}</td>
  <td class="py-3 px-3 text-sm text-slate-500">{}</td>
  <td class="py-3 px-3">
    <button hx-delete="/admin/api/tokens"
      hx-vals='{{"token": "{}"}}'
      hx-confirm="Delete token {}?"
      hx-swap="delete"
      class="text-slate-400 hover:text-red-500 p-1 rounded hover:bg-red-50 transition-colors"
      title="Delete">
      <svg class="w-4 h-4" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" d="M14.74 9l-.346 9m-4.788 0L9.26 9m9.968-3.21c.342.052.682.107 1.022.166m-1.022-.165L18.16 19.673a2.25 2.25 0 01-2.244 2.077H8.084a2.25 2.25 0 01-2.244-2.077L4.772 5.79m14.456 0a48.108 48.108 0 00-3.478-.397m-12 .562c.34-.059.68-.114 1.022-.165m0 0a48.11 48.11 0 013.478-.397m7.5 0v-.916c0-1.18-.91-2.164-2.09-2.201a51.964 51.964 0 00-3.32 0c-1.18.037-2.09 1.022-2.09 2.201v.916m7.5 0a48.667 48.667 0 00-7.5 0"/></svg>
    </button>
  </td>
</tr>"##,
            t.token, t.token, status_class, status_text,
            t.created_at.format("%Y-%m-%d"), expires_str,
            t.token, t.token
        )
    }).collect();
    ok_html(rows.join(""))
}

pub async fn render_form_new(_app: &Arc<App>, csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    let html = crate::templates::TokenFormTemplate { token: None, error: None }
        .render()
        .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

pub async fn handle_create(app: &Arc<App>, body: &[u8], csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    let params = parse_form(body);
    let token = params.get("token").cloned().unwrap_or_default();
    let expires_at = params.get("expires_at").and_then(|s| {
        if s.is_empty() { None } else {
            chrono::DateTime::parse_from_str(&format!("{}T00:00:00Z", s), "%Y-%m-%dT%H:%M:%SZ").ok().map(|dt| dt.with_timezone(&chrono::Utc))
        }
    });

    if token.is_empty() {
        return render_form_with_error(None, "Token name is required", csrf);
    }
    if !pangolin_core::is_valid_tun_name(&token) {
        return render_form_with_error(None, "Token name must be lowercase letters, digits, or hyphens (1-32 chars)", csrf);
    }

    let t = pangolin_core::types::Token {
        token: token.clone(),
        enabled: true,
        created_at: chrono::Utc::now(),
        expires_at,
    };

    let db = app.db.lock().await;
    let result = pangolin_core::db::upsert_token(&db, &t);
    drop(db);

    match result {
        Ok(()) => {
            app.reload_indexes().await;
            ok_html(r##"<div id="toast" class="fixed bottom-4 right-4 z-50"><div class="bg-green-600 text-white px-4 py-2 rounded-lg shadow-lg text-sm">Token created</div></div>"##.to_string())
        }
        Err(e) => render_form_with_error(None, &format!("Database error: {}", e), csrf),
    }
}

fn render_form_with_error(token: Option<pangolin_core::types::Token>, error: &str, csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    let html = crate::templates::TokenFormTemplate { token, error: Some(error) }
        .render()
        .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

pub async fn handle_delete(app: &Arc<App>, token: Option<String>, _csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    let token = match token {
        Some(t) if !t.is_empty() => t,
        _ => return ok_html(r##"<p class="text-red-500 text-sm">Missing token</p>"##.to_string()),
    };
    let db = app.db.lock().await;
    let result = pangolin_core::db::delete_token(&db, &token);
    drop(db);

    match result {
        Ok(true) => {
            app.reload_indexes().await;
            ok_html(r##"<div id="toast" class="fixed bottom-4 right-4 z-50"><div class="bg-slate-700 text-white px-4 py-2 rounded-lg shadow-lg text-sm">Token deleted</div></div>"##.to_string())
        }
        Ok(false) => ok_html(r##"<p class="text-red-500 text-sm">Token not found</p>"##.to_string()),
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