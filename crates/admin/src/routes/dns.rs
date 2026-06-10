//! DNS provider admin route — list / new / edit / delete.

use askama::Template;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Context;

use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use crate::templates::{DnsProviderFormTemplate, DnsProvidersTemplate};
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
    let providers = {
        let db = app.db.lock().await;
        pangolin_core::db::list_dns_providers(&db).unwrap_or_default()
    };
    // Count how many domains reference each provider.
    let mut domain_counts: HashMap<String, usize> = HashMap::new();
    {
        let db = app.db.lock().await;
        for d in pangolin_core::db::list_domains(&db).unwrap_or_default() {
            if let Some(p) = d.dns_provider {
                *domain_counts.entry(p).or_insert(0) += 1;
            }
        }
    }
    ok_html(crate::render_with_assets_and_csrf(
        DnsProvidersTemplate {
            providers,
            domain_counts,
            active_nav: "dns",
        }
        .render()
        .unwrap(),
        csrf,
    ))
}

pub async fn render_create_page(
    _app: &Arc<App>,
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    let html = DnsProviderFormTemplate {
        provider: None,
        action: "/admin/dns/new",
        form_title: "New DNS Provider",
        submit_label: "Create",
        is_edit: false,
        error: None,
        active_nav: "dns",
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

pub async fn render_edit_page(
    app: &Arc<App>,
    name: Option<String>,
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    let Some(name) = name else {
        return Ok(crate::not_found());
    };
    let provider = {
        let db = app.db.lock().await;
        pangolin_core::db::get_dns_provider(&db, &name).unwrap_or(None)
    };
    let Some(provider) = provider else {
        return Ok(crate::not_found());
    };
    let html = DnsProviderFormTemplate {
        provider: Some(provider),
        action: "/admin/dns/edit",
        form_title: "Edit DNS Provider",
        submit_label: "Save",
        is_edit: true,
        error: None,
        active_nav: "dns",
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
    let name = params.get("name").cloned().unwrap_or_default();
    let kind_str = params.get("kind").cloned().unwrap_or_default();
    let enabled = params.get("enabled").map(|_| true).unwrap_or(false);
    let config = params.get("config").cloned().unwrap_or_default();

    if name.is_empty() {
        return render_create_page_with_error(app, "Name is required", csrf);
    }
    if !is_valid_dns_name(&name) {
        return render_create_page_with_error(
            app,
            "Name must be lowercase letters, digits, underscore, or dash (1-64 chars)",
            csrf,
        );
    }
    let kind = match pangolin_core::DnsProviderKind::from_str(&kind_str) {
        Ok(k) => k,
        Err(e) => return render_create_page_with_error(app, &e, csrf),
    };
    if config.trim().is_empty() {
        return render_create_page_with_error(app, "Config (credentials JSON) is required", csrf);
    }
    // Validate JSON shape.
    if let Err(e) = serde_json::from_str::<serde_json::Value>(&config) {
        return render_create_page_with_error(app, &format!("Config is not valid JSON: {e}"), csrf);
    }

    let now = chrono::Utc::now();
    let p = pangolin_core::types::DnsProvider {
        name: name.clone(),
        kind,
        enabled,
        config,
        created_at: now,
        updated_at: now,
    };
    let db = app.db.lock().await;
    let result = pangolin_core::db::upsert_dns_provider(&db, &p);
    drop(db);
    match result {
        Ok(()) => {
            app.reload_indexes().await;
            Ok(redirect_response("/admin/dns"))
        }
        Err(e) => render_create_page_with_error(app, &format!("Database error: {e}"), csrf),
    }
}

pub async fn handle_update(
    app: &Arc<App>,
    name: Option<String>,
    body: &[u8],
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    let Some(name) = name else {
        return Ok(crate::not_found());
    };
    let params = parse_form(body);
    let kind_str = params.get("kind").cloned().unwrap_or_default();
    let enabled = params.get("enabled").map(|_| true).unwrap_or(false);
    let config = params.get("config").cloned().unwrap_or_default();

    let kind = match pangolin_core::DnsProviderKind::from_str(&kind_str) {
        Ok(k) => k,
        Err(e) => {
            return Ok(error_response(&e));
        }
    };
    if config.trim().is_empty() {
        return Ok(error_response("Config (credentials JSON) is required"));
    }
    if let Err(e) = serde_json::from_str::<serde_json::Value>(&config) {
        return Ok(error_response(&format!("Config is not valid JSON: {e}")));
    }

    let existing = {
        let db = app.db.lock().await;
        pangolin_core::db::get_dns_provider(&db, &name).unwrap_or(None)
    };
    let Some(existing) = existing else {
        return Ok(crate::not_found());
    };

    let updated = pangolin_core::types::DnsProvider {
        name: name.clone(),
        kind,
        enabled,
        config,
        created_at: existing.created_at,
        updated_at: chrono::Utc::now(),
    };
    let db = app.db.lock().await;
    let result = pangolin_core::db::upsert_dns_provider(&db, &updated);
    drop(db);
    match result {
        Ok(()) => {
            app.reload_indexes().await;
            Ok(redirect_response("/admin/dns"))
        }
        Err(e) => Ok(error_response(&format!("Database error: {e}"))),
    }
}

/// Delete a DNS provider. The handler runs the schema's logical
/// `ON DELETE SET NULL` in a transaction: clear `domains.dns_provider`
/// for any row that references this name, then delete the provider.
/// This is the v2 design's "use code, not sqlite cascades" rule.
pub async fn handle_delete(
    app: &Arc<App>,
    name: Option<String>,
    _csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    let Some(name) = name else {
        return Ok(crate::not_found());
    };
    match delete_provider_txn(app, &name).await {
        Ok(upd) => {
            log::info!(
                "Deleted DNS provider '{}' (cleared {} domain references)",
                name,
                upd
            );
            app.reload_indexes().await;
            Ok(redirect_response("/admin/dns"))
        }
        Err(e) => Ok(error_response(&format!("delete failed: {e}"))),
    }
}

/// Run the v2 transactional delete: clear `domains.dns_provider` for
/// any row that references the given name, then delete the provider.
async fn delete_provider_txn(app: &Arc<App>, name: &str) -> anyhow::Result<usize> {
    let mut conn = app.db.lock().await;
    let tx = conn.transaction().context("begin tx")?;
    let upd = tx
        .execute(
            "UPDATE domains SET dns_provider = NULL WHERE dns_provider = ?1",
            rusqlite::params![name],
        )
        .context("update domains")?;
    tx.execute(
        "DELETE FROM dns_providers WHERE name = ?1",
        rusqlite::params![name],
    )
    .context("delete provider")?;
    tx.commit().context("commit")?;
    Ok(upd)
}

fn render_create_page_with_error(
    _app: &Arc<App>,
    err: &str,
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    let html = DnsProviderFormTemplate {
        provider: None,
        action: "/admin/dns/new",
        form_title: "New DNS Provider",
        submit_label: "Create",
        is_edit: false,
        error: Some(err),
        active_nav: "dns",
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

fn error_response(msg: &str) -> Response<Full<Bytes>> {
    let body = format!("error: {}", msg);
    Response::builder()
        .status(400)
        .header("Content-Type", "text/plain")
        .body(Full::new(Bytes::from(body)))
        .expect("400 response builder is infallible")
}

fn http_error_response_unused() {}

fn is_valid_dns_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
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
