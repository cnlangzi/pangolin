//! DNS provider POST / DELETE / test handlers.

use anyhow::Context;
use askama::Template;
use std::str::FromStr;
use std::sync::Arc;

use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use pangolin_core::DnsProviderKind;

use crate::{App, redirect_response};

use super::helpers::{is_valid_dns_name, parse_form};
use super::pages::build_new_form;

type Resp = Response<Full<Bytes>>;

pub async fn handle_create(app: &Arc<App>, body: &[u8], csrf: &str) -> http::Result<Resp> {
    let params = parse_form(body);
    let name = params.get("name").cloned().unwrap_or_default();
    let kind_str = params.get("kind").cloned().unwrap_or_default();
    let enabled = params.get("enabled").map(|_| true).unwrap_or(false);

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

    let config = match assemble_config(kind, &params, None) {
        Ok(c) => c,
        Err(e) => return render_create_page_with_error(app, &e, csrf),
    };

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
            Ok(redirect_response("/dns"))
        }
        Err(e) => render_create_page_with_error(app, &format!("Database error: {e}"), csrf),
    }
}

pub async fn handle_update(
    app: &Arc<App>,
    name: Option<String>,
    body: &[u8],
    _csrf: &str,
) -> http::Result<Resp> {
    let Some(name) = name else {
        return Ok(crate::not_found());
    };
    let params = parse_form(body);
    let kind_str = params.get("kind").cloned().unwrap_or_default();
    let enabled = params.get("enabled").map(|_| true).unwrap_or(false);

    let kind = match pangolin_core::DnsProviderKind::from_str(&kind_str) {
        Ok(k) => k,
        Err(e) => return Ok(error_response(&e)),
    };

    let existing = {
        let db = app.db.lock().await;
        pangolin_core::db::get_dns_provider(&db, &name).unwrap_or(None)
    };
    let Some(existing) = existing else {
        return Ok(crate::not_found());
    };

    let config = match assemble_config(kind, &params, Some(&existing.config)) {
        Ok(c) => c,
        Err(e) => return Ok(error_response(&e)),
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
            Ok(redirect_response("/dns"))
        }
        Err(e) => Ok(error_response(&format!("Database error: {e}"))),
    }
}

pub async fn handle_test(app: &Arc<App>, body: &[u8]) -> http::Result<Resp> {
    let params = parse_form(body);
    let kind_str = params.get("kind").cloned().unwrap_or_default();
    let kind = match DnsProviderKind::from_str(&kind_str) {
        Ok(k) => k,
        Err(e) => {
            return Ok(test_json(false, &format!("Invalid provider kind: {e}")));
        }
    };

    let name = params.get("name").cloned().unwrap_or_default();
    let existing_config = if !name.is_empty() {
        let db = app.db.lock().await;
        pangolin_core::db::get_dns_provider(&db, &name)
            .ok()
            .flatten()
            .map(|p| p.config)
    } else {
        None
    };

    let config = match assemble_config(kind, &params, existing_config.as_deref()) {
        Ok(c) => c,
        Err(e) => return Ok(test_json(false, &e)),
    };

    if let Err(e) = static_validate_config(kind, &config) {
        return Ok(test_json(false, &e));
    }
    Ok(test_json(true, ""))
}

fn test_json(ok: bool, error: &str) -> Response<Full<Bytes>> {
    let body = if ok {
        serde_json::json!({ "ok": true }).to_string()
    } else {
        serde_json::json!({ "ok": false, "error": error }).to_string()
    };
    Response::builder()
        .status(if ok { 200 } else { 400 })
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .expect("test_json response builder infallible")
}

// NOTE: `handle_delete` is the legacy form-POST endpoint. It will be removed
// once all admin UIs migrate to the HTMX `hx-delete` button (see
// `api_handle_delete` below and the `templates/components/_hx_delete_button.html`
// partial). Tracked by issue #48.
pub async fn handle_delete(
    app: &Arc<App>,
    name: Option<String>,
    _csrf: &str,
) -> http::Result<Resp> {
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
            Ok(redirect_response("/dns"))
        }
        Err(e) => Ok(error_response(&format!("delete failed: {e}"))),
    }
}

/// HTMX `DELETE /api/dns/{name}` — returns an empty 200 body so HTMX
/// (with `hx-swap="delete"`) can drop the row without a full page reload.
///
/// This is the unified delete endpoint for DNS providers; the form-POST
/// `/dns/{name}/delete` route above is kept for now as a fallback during
/// the migration window (issue #48).
pub async fn api_handle_delete(app: &Arc<App>, name: String, _csrf: &str) -> http::Result<Resp> {
    if name.is_empty() {
        return Ok(crate::not_found());
    }
    match delete_provider_txn(app, &name).await {
        Ok(upd) => {
            log::info!(
                "Deleted DNS provider '{}' (cleared {} domain references)",
                name,
                upd
            );
            app.reload_indexes().await;
            Ok(Response::builder()
                .status(200)
                .header("Content-Type", "text/html; charset=utf-8")
                .body(Full::new(Bytes::new()))
                .unwrap())
        }
        Err(e) => Ok(error_response(&format!("delete failed: {e}"))),
    }
}

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

fn render_create_page_with_error(_app: &Arc<App>, err: &str, _csrf: &str) -> http::Result<Resp> {
    let html = build_new_form(Some(err)).render().unwrap();
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(html)))
        .expect("response builder for 200 OK should not fail"))
}

fn static_validate_config(kind: DnsProviderKind, config: &str) -> Result<(), String> {
    let v: serde_json::Value =
        serde_json::from_str(config).map_err(|e| format!("config is not valid JSON: {e}"))?;
    let non_empty = |s: &str| !s.is_empty();
    match kind {
        DnsProviderKind::Cloudflare => {
            let t = v.get("api_token").and_then(|x| x.as_str()).unwrap_or("");
            if !non_empty(t) {
                return Err("api_token is required".into());
            }
        }
        DnsProviderKind::Aliyun => {
            let ak = v
                .get("access_key_id")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            let sk = v
                .get("access_key_secret")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            if !non_empty(ak) || !non_empty(sk) {
                return Err("access_key_id and access_key_secret are required".into());
            }
        }
        DnsProviderKind::Tencent => {
            let id = v.get("secret_id").and_then(|x| x.as_str()).unwrap_or("");
            let key = v.get("secret_key").and_then(|x| x.as_str()).unwrap_or("");
            if !non_empty(id) || !non_empty(key) {
                return Err("secret_id and secret_key are required".into());
            }
        }
    }
    Ok(())
}

fn error_response(msg: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(400)
        .header("Content-Type", "text/plain")
        .body(Full::new(Bytes::from(format!("error: {}", msg))))
        .expect("400 response builder is infallible")
}

fn assemble_config(
    kind: DnsProviderKind,
    params: &std::collections::HashMap<String, String>,
    existing_config: Option<&str>,
) -> Result<String, String> {
    let existing: serde_json::Value = existing_config
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_else(|| serde_json::json!({}));

    let non_empty =
        |s: &str| -> bool { !s.is_empty() && s != "••••••••••••" };

    let merge_secret =
        |param_name: &str, existing_field: &str, label: &str| -> Result<String, String> {
            let submitted = params.get(param_name).cloned().unwrap_or_default();
            if non_empty(&submitted) {
                Ok(submitted)
            } else if let Some(prev) = existing.get(existing_field).and_then(|x| x.as_str()) {
                Ok(prev.to_string())
            } else {
                Err(format!("{} is required", label))
            }
        };

    let v = match kind {
        DnsProviderKind::Cloudflare => {
            let token = merge_secret("api_token", "api_token", "API token")?;
            serde_json::json!({ "api_token": token })
        }
        DnsProviderKind::Aliyun => {
            let ak = params.get("access_key_id").cloned().unwrap_or_default();
            if ak.is_empty() {
                return Err("Access Key ID is required".to_string());
            }
            let sk = merge_secret(
                "access_key_secret",
                "access_key_secret",
                "Access Key Secret",
            )?;
            let region = params
                .get("region")
                .cloned()
                .unwrap_or_else(|| "cn-hangzhou".into());
            serde_json::json!({
                "access_key_id": ak,
                "access_key_secret": sk,
                "region": region,
            })
        }
        DnsProviderKind::Tencent => {
            let id = params.get("secret_id").cloned().unwrap_or_default();
            if id.is_empty() {
                return Err("Secret ID is required".to_string());
            }
            let key = merge_secret("secret_key", "secret_key", "Secret Key")?;
            serde_json::json!({
                "secret_id": id,
                "secret_key": key,
            })
        }
    };

    serde_json::to_string(&v).map_err(|e| format!("failed to serialize config: {e}"))
}
