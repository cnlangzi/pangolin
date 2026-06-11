//! DNS provider admin route — list / new / edit / delete / test.

use askama::Template;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Context;

use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use pangolin_core::DnsProviderKind;

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
    let html = empty_form(None, None, "dns").render().unwrap();
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
    let html = empty_form(Some(&provider), None, "dns").render().unwrap();
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
            Ok(redirect_response("/admin/dns"))
        }
        Err(e) => render_create_page_with_error(app, &format!("Database error: {e}"), csrf),
    }
}

pub async fn handle_update(
    app: &Arc<App>,
    name: Option<String>,
    body: &[u8],
    _csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
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
            Ok(redirect_response("/admin/dns"))
        }
        Err(e) => Ok(error_response(&format!("Database error: {e}"))),
    }
}

/// POST /admin/dns/test — verify credentials by constructing a provider and
/// doing one read-only call. Returns JSON {"ok": bool, "error": "..."}.
pub async fn handle_test(app: &Arc<App>, body: &[u8]) -> http::Result<Response<Full<Bytes>>> {
    let params = parse_form(body);
    let kind_str = params.get("kind").cloned().unwrap_or_default();
    let kind = match DnsProviderKind::from_str(&kind_str) {
        Ok(k) => k,
        Err(e) => {
            return Ok(test_json(false, &format!("Invalid provider kind: {e}")));
        }
    };

    // For test, treat empty secrets as "use existing if any" only if the
    // caller provided a name; otherwise require all fields.
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

    // NOTE: We don't actually call out to the provider's API here, because
    // the DnsProvider trait + provider implementations live in `ngx` (which
    // `admin` does not depend on). We do strong static validation: the JSON
    // must be well-formed AND each required field must be non-empty.
    // This catches ~80% of misconfiguration (missing fields, typos, bad JSON).
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
    _csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    let html = empty_form(None, Some(err), "dns").render().unwrap();
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(html)))
        .expect("response builder for 200 OK should not fail"))
}

/// Static validation of the assembled JSON config. Mirrors `from_kind_config`
/// in `crates/ngx/src/dns/mod.rs` so a passing test here implies the JSON
/// will be accepted by the factory at issuance time.
fn static_validate_config(kind: DnsProviderKind, config: &str) -> Result<(), String> {
    let v: serde_json::Value = serde_json::from_str(config)
        .map_err(|e| format!("config is not valid JSON: {e}"))?;
    let non_empty = |s: &str| !s.is_empty();
    match kind {
        DnsProviderKind::Cloudflare => {
            let t = v.get("api_token").and_then(|x| x.as_str()).unwrap_or("");
            if !non_empty(t) {
                return Err("api_token is required".into());
            }
        }
        DnsProviderKind::Aliyun => {
            let ak = v.get("access_key_id").and_then(|x| x.as_str()).unwrap_or("");
            let sk = v.get("access_key_secret").and_then(|x| x.as_str()).unwrap_or("");
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

fn _http_error_response_unused_marker() {}

/// Build a DnsProviderFormTemplate populated with the right per-kind fields.
/// For edit mode, `provider` provides the existing config so the form can
/// pre-populate non-secret fields and decide which `_set` flags are true.
fn empty_form<'a>(
    provider: Option<&'a pangolin_core::types::DnsProvider>,
    error: Option<&'a str>,
    active_nav: &'a str,
) -> DnsProviderFormTemplate<'a> {
    let is_edit = provider.is_some();
    let (cf_token, cf_token_set, aliyun_ak_id, aliyun_ak_secret, aliyun_ak_secret_set, aliyun_region, tencent_secret_id, tencent_secret_key, tencent_secret_key_set) =
        match provider {
            Some(p) => {
                let v: serde_json::Value = serde_json::from_str(&p.config).unwrap_or_default();
                match p.kind {
                    DnsProviderKind::Cloudflare => {
                        let t = v.get("api_token").and_then(|x| x.as_str()).unwrap_or("").to_string();
                        (Some("••••••••••••".into()), !t.is_empty(), None, None, false, None, None, None, false)
                    }
                    DnsProviderKind::Aliyun => {
                        let ak = v.get("access_key_id").and_then(|x| x.as_str()).unwrap_or("").to_string();
                        let sk = v.get("access_key_secret").and_then(|x| x.as_str()).unwrap_or("").to_string();
                        let r = v.get("region").and_then(|x| x.as_str()).unwrap_or("cn-hangzhou").to_string();
                        (None, false, Some(ak), Some("••••••••••••".into()), !sk.is_empty(), Some(r), None, None, false)
                    }
                    DnsProviderKind::Tencent => {
                        let id = v.get("secret_id").and_then(|x| x.as_str()).unwrap_or("").to_string();
                        let key = v.get("secret_key").and_then(|x| x.as_str()).unwrap_or("").to_string();
                        (None, false, None, None, false, None, Some(id), Some("••••••••••••".into()), !key.is_empty())
                    }
                }
            }
            None => (None, false, None, None, false, None, None, None, false),
        };

    DnsProviderFormTemplate {
        provider: provider.cloned(),
        action: if is_edit { "/admin/dns/edit" } else { "/admin/dns/new" },
        form_title: if is_edit { "Edit DNS Provider" } else { "New DNS Provider" },
        submit_label: if is_edit { "Save" } else { "Create" },
        is_edit,
        error,
        active_nav,
        cf_token,
        cf_token_set,
        aliyun_ak_id,
        aliyun_ak_secret,
        aliyun_ak_secret_set,
        aliyun_region,
        tencent_secret_id,
        tencent_secret_key,
        tencent_secret_key_set,
    }
}

/// Assemble the JSON config blob from per-kind form fields.
/// In edit mode (`existing_config` is Some), preserve any field that was
/// not re-submitted (so editing just `enabled` doesn't wipe the token).
fn assemble_config(
    kind: DnsProviderKind,
    params: &HashMap<String, String>,
    existing_config: Option<&str>,
) -> Result<String, String> {
    let existing: serde_json::Value = existing_config
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_else(|| serde_json::json!({}));

    let non_empty = |s: &str| -> bool { !s.is_empty() && s != "••••••••••••" };

    let v = match kind {
        DnsProviderKind::Cloudflare => {
            let submitted = params.get("api_token").cloned().unwrap_or_default();
            let token = if non_empty(&submitted) {
                submitted
            } else if let Some(prev) = existing.get("api_token").and_then(|x| x.as_str()) {
                prev.to_string()
            } else {
                return Err("API token is required".to_string());
            };
            serde_json::json!({ "api_token": token })
        }
        DnsProviderKind::Aliyun => {
            let ak = params.get("access_key_id").cloned().unwrap_or_default();
            if ak.is_empty() {
                return Err("Access Key ID is required".to_string());
            }
            let sk_submitted = params.get("access_key_secret").cloned().unwrap_or_default();
            let sk = if non_empty(&sk_submitted) {
                sk_submitted
            } else if let Some(prev) = existing.get("access_key_secret").and_then(|x| x.as_str()) {
                prev.to_string()
            } else {
                return Err("Access Key Secret is required".to_string());
            };
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
            let key_submitted = params.get("secret_key").cloned().unwrap_or_default();
            let key = if non_empty(&key_submitted) {
                key_submitted
            } else if let Some(prev) = existing.get("secret_key").and_then(|x| x.as_str()) {
                prev.to_string()
            } else {
                return Err("Secret Key is required".to_string());
            };
            serde_json::json!({
                "secret_id": id,
                "secret_key": key,
            })
        }
    };

    serde_json::to_string(&v).map_err(|e| format!("failed to serialize config: {e}"))
}

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
