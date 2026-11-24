//! DNS provider POST / DELETE / test handlers.

use anyhow::Context;
use askama::Template;

use crate::i18n::Lang;
use std::str::FromStr;
use std::sync::Arc;

use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use pangolin_core::DnsProviderKind;

use crate::{App, redirect_response};

use super::helpers::is_valid_dns_name;
use super::pages::build_new_form;
use crate::routes::helpers::parse_form;

use std::collections::HashMap;

type Resp = Response<Full<Bytes>>;

/// The parsed contents of a DNS provider form.
pub struct DnsProviderForm {
    pub name: String,
    pub kind: DnsProviderKind,
    pub enabled: bool,
    /// The kind-specific credential blob, as JSON.
    pub config: String,
}

/// Parse and validate a DNS provider form.
///
/// The same parser serves the HTML form and the dashboard-facing JSON
/// API (issue #185) — a submission the form rejects is rejected here
/// too, with the same message.
///
/// * `name` — the provider's name: from the form on create, from the
///   path on update.
/// * `existing` — the stored row on update, `None` on create. Its
///   credentials are what a submission that omits a secret falls back
///   to.
pub fn parse_dns_provider_form(
    params: &HashMap<String, String>,
    name: &str,
    existing: Option<&pangolin_core::types::DnsProvider>,
    lang: Lang,
) -> Result<DnsProviderForm, String> {
    // The name is the primary key, so it is only checked when the row
    // is created — an edit cannot change it.
    if existing.is_none() {
        if name.is_empty() {
            return Err(lang.t("dns.error.name_required").into_owned());
        }
        if !is_valid_dns_name(name) {
            return Err(lang.t("dns.error.name_format").into_owned());
        }
    }
    let kind_str = params.get("kind").cloned().unwrap_or_default();
    let kind = DnsProviderKind::from_str(&kind_str)?;
    let enabled = params.get("enabled").map(|_| true).unwrap_or(false);
    let config = assemble_config(kind, params, existing.map(|p| p.config.as_str()), lang)?;
    Ok(DnsProviderForm {
        name: name.to_string(),
        kind,
        enabled,
        config,
    })
}

pub async fn handle_create(
    app: &Arc<App>,
    body: &[u8],
    csrf: &str,
    lang: Lang,
) -> http::Result<Resp> {
    let params = parse_form(body);
    let name = params.get("name").cloned().unwrap_or_default();

    let parsed = match parse_dns_provider_form(&params, &name, None, lang) {
        Ok(p) => p,
        Err(e) => return render_create_page_with_error(app, &e, csrf, lang),
    };

    let now = chrono::Utc::now();
    let p = pangolin_core::types::DnsProvider {
        name: parsed.name,
        kind: parsed.kind,
        enabled: parsed.enabled,
        config: parsed.config,
        created_at: now,
        updated_at: now,
    };
    // The write runs on the blocking pool (issue #254).
    let result = app
        .with_db_res(move |db| pangolin_core::db::upsert_dns_provider(db, &p))
        .await;
    match result {
        Ok(()) => {
            app.reload_indexes().await;
            Ok(redirect_response("/dns"))
        }
        Err(e) => render_create_page_with_error(app, &format!("Database error: {e}"), csrf, lang),
    }
}

pub async fn handle_update(
    app: &Arc<App>,
    name: Option<String>,
    body: &[u8],
    _csrf: &str,
    lang: Lang,
) -> http::Result<Resp> {
    let Some(name) = name else {
        return Ok(crate::not_found(lang));
    };
    let params = parse_form(body);

    let existing = {
        // The read runs on the blocking pool (issue #254).
        let key = name.clone();
        app.with_db_or(move |db| pangolin_core::db::get_dns_provider(db, &key).unwrap_or(None))
            .await
    };
    let Some(existing) = existing else {
        return Ok(crate::not_found(lang));
    };

    let parsed = match parse_dns_provider_form(&params, &name, Some(&existing), lang) {
        Ok(p) => p,
        Err(e) => return Ok(error_response(&e)),
    };

    let updated = pangolin_core::types::DnsProvider {
        name: parsed.name,
        kind: parsed.kind,
        enabled: parsed.enabled,
        config: parsed.config,
        created_at: existing.created_at,
        updated_at: chrono::Utc::now(),
    };
    // The write runs on the blocking pool (issue #254).
    let result = app
        .with_db_res(move |db| pangolin_core::db::upsert_dns_provider(db, &updated))
        .await;
    match result {
        Ok(()) => {
            app.reload_indexes().await;
            Ok(redirect_response("/dns"))
        }
        Err(e) => Ok(error_response(&format!("Database error: {e}"))),
    }
}

pub async fn handle_test(app: &Arc<App>, body: &[u8], lang: Lang) -> http::Result<Resp> {
    let params = parse_form(body);
    let kind_str = params.get("kind").cloned().unwrap_or_default();
    let kind = match DnsProviderKind::from_str(&kind_str) {
        Ok(k) => k,
        Err(e) => {
            return Ok(test_json(
                false,
                &lang.tf("dns.error.invalid_kind", [("error", e.to_string())]),
            ));
        }
    };

    let name = params.get("name").cloned().unwrap_or_default();
    let existing_config = if !name.is_empty() {
        // The read runs on the blocking pool (issue #254).
        let key = name.clone();
        app.with_db_or(move |db| {
            pangolin_core::db::get_dns_provider(db, &key)
                .ok()
                .flatten()
                .map(|p| p.config)
        })
        .await
    } else {
        None
    };

    let config = match assemble_config(kind, &params, existing_config.as_deref(), lang) {
        Ok(c) => c,
        Err(e) => return Ok(test_json(false, &e)),
    };

    if let Err(e) = static_validate_config(kind, &config, lang) {
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
    lang: Lang,
) -> http::Result<Resp> {
    let Some(name) = name else {
        return Ok(crate::not_found(lang));
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
pub async fn api_handle_delete(
    app: &Arc<App>,
    name: String,
    _csrf: &str,
    lang: Lang,
) -> http::Result<Resp> {
    if name.is_empty() {
        return Ok(crate::not_found(lang));
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
    // The whole delete runs on the blocking pool, inside one
    // transaction: the domains that referenced the provider and the
    // provider row itself are removed together (issue #254).
    let name = name.to_string();
    app.with_db(move |conn| {
        let tx = conn.unchecked_transaction().context("begin tx")?;
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
    })
    .await
    .map_err(pangolin_core::DbError::from)?
}

fn render_create_page_with_error(
    _app: &Arc<App>,
    err: &str,
    _csrf: &str,
    lang: Lang,
) -> http::Result<Resp> {
    let html = build_new_form(Some(err), lang).render().unwrap();
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(html)))
        .expect("response builder for 200 OK should not fail"))
}

fn static_validate_config(kind: DnsProviderKind, config: &str, lang: Lang) -> Result<(), String> {
    let v: serde_json::Value = serde_json::from_str(config)
        .map_err(|e| lang.tf("dns.error.config_not_json", [("error", e.to_string())]))?;
    let non_empty = |s: &str| !s.is_empty();
    match kind {
        DnsProviderKind::Cloudflare => {
            let t = v.get("api_token").and_then(|x| x.as_str()).unwrap_or("");
            if !non_empty(t) {
                return Err(lang.tf("dns.error.field_required", [("field", "api_token")]));
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
                return Err(lang.tf(
                    "dns.error.fields_required",
                    [("fields", "access_key_id and access_key_secret")],
                ));
            }
        }
        DnsProviderKind::Tencent => {
            let id = v.get("secret_id").and_then(|x| x.as_str()).unwrap_or("");
            let key = v.get("secret_key").and_then(|x| x.as_str()).unwrap_or("");
            if !non_empty(id) || !non_empty(key) {
                return Err(lang.tf(
                    "dns.error.fields_required",
                    [("fields", "secret_id and secret_key")],
                ));
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

/// Merge the submitted credentials with the stored ones into the
/// kind-specific JSON blob the `dns_providers.config` column holds.
///
/// A secret the submission does not carry keeps the stored value; a
/// provider that has none and gets none is an error.
pub fn assemble_config(
    kind: DnsProviderKind,
    params: &std::collections::HashMap<String, String>,
    existing_config: Option<&str>,
    lang: Lang,
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
                Err(lang.tf("dns.error.field_required", [("field", label)]))
            }
        };

    let v = match kind {
        DnsProviderKind::Cloudflare => {
            let token = merge_secret(
                "api_token",
                "api_token",
                &lang.t("dns.error.label_api_token"),
            )?;
            serde_json::json!({ "api_token": token })
        }
        DnsProviderKind::Aliyun => {
            let ak = params.get("access_key_id").cloned().unwrap_or_default();
            if ak.is_empty() {
                return Err(lang.t("dns.error.access_key_id_required").into_owned());
            }
            let sk = merge_secret(
                "access_key_secret",
                "access_key_secret",
                &lang.t("dns.error.label_access_key_secret"),
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
                return Err(lang.t("dns.error.secret_id_required").into_owned());
            }
            let key = merge_secret(
                "secret_key",
                "secret_key",
                &lang.t("dns.error.label_secret_key"),
            )?;
            serde_json::json!({
                "secret_id": id,
                "secret_key": key,
            })
        }
    };

    serde_json::to_string(&v).map_err(|e| format!("failed to serialize config: {e}"))
}
