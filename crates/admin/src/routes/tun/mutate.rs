//! Tunnels POST / DELETE handlers.

use std::sync::Arc;

use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use crate::{App, redirect_response};
use pangolin_core::types::Tun;

use super::helpers::{generate_token, parse_datetime};
use super::pages::{render_create_page_with_error, render_edit_page_with_error};
use crate::i18n::Lang;
use crate::routes::helpers::{bad_request_page, parse_form};

type Resp = Response<Full<Bytes>>;

pub async fn handle_create(
    app: &Arc<App>,
    body: &[u8],
    csrf: &str,
    lang: Lang,
) -> http::Result<Resp> {
    let params = parse_form(body);
    let name = params.get("name").cloned().unwrap_or_default();

    if name.is_empty() {
        return render_create_page_with_error(
            None,
            &lang.t("tunnels.error.name_required"),
            csrf,
            lang,
        );
    }
    if name.len() > 64 {
        return render_create_page_with_error(
            None,
            &lang.t("tunnels.error.name_too_long"),
            csrf,
            lang,
        );
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphabetic() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        return render_create_page_with_error(
            None,
            &lang.t("tunnels.error.name_charset"),
            csrf,
            lang,
        );
    }

    let token = params
        .get("token")
        .cloned()
        .filter(|t| !t.is_empty())
        .map(Ok)
        .unwrap_or_else(generate_token);

    let token = match token {
        Ok(t) => t,
        Err(e) => {
            return render_create_page_with_error(
                None,
                &format!("Token generation error: {}", e),
                csrf,
                lang,
            );
        }
    };

    {
        let db = app.db.lock().await;
        if pangolin_core::db::get_tun(&db, &name)
            .unwrap_or_default()
            .is_some()
        {
            drop(db);
            return render_create_page_with_error(
                None,
                &lang.tf("tunnels.error.already_exists", [("name", name)]),
                csrf,
                lang,
            );
        }
    }

    let enabled = params.get("enabled").map(|v| v == "1").unwrap_or(true);
    let expires_at = parse_datetime(params.get("expires_at").cloned().as_deref());

    let tun = Tun {
        name: name.clone(),
        token: Some(token),
        token_hash: None,
        enabled,
        online: false,
        registered_at: None,
        last_seen_at: None,
        expires_at,
    };

    let db = app.db.lock().await;
    let result = pangolin_core::db::upsert_tun(&db, &tun);
    drop(db);

    match result {
        Ok(()) => Ok(redirect_response("/tun")),
        Err(e) => {
            render_create_page_with_error(None, &format!("Database error: {}", e), csrf, lang)
        }
    }
}

pub async fn handle_update(
    app: &Arc<App>,
    name: Option<String>,
    body: &[u8],
    csrf: &str,
    lang: Lang,
) -> http::Result<Resp> {
    let name = match name {
        Some(n) if !n.is_empty() => n,
        _ => {
            return Ok(bad_request_page(
                lang,
                &lang.t("tunnels.error.missing_name"),
                "/tun",
                &lang.t("common.back_to_tunnels"),
            ));
        }
    };

    let params = parse_form(body);

    let db = app.db.lock().await;
    let existing = pangolin_core::db::get_tun(&db, &name).unwrap_or_default();
    drop(db);

    let Some(mut tun) = existing else {
        return render_edit_page_with_error(&name, &lang.t("tunnels.error.not_found"), csrf, lang);
    };

    if let Some(new_token) = params.get("token").cloned().filter(|t| !t.is_empty()) {
        tun.token = Some(new_token);
    }

    if let Some(raw) = params.get("expires_at").cloned() {
        if raw.is_empty() {
            tun.expires_at = None;
        } else {
            tun.expires_at = parse_datetime(Some(raw.as_str()));
        }
    }

    tun.enabled = params
        .get("enabled")
        .map(|v| v == "1")
        .unwrap_or(tun.enabled);

    let db = app.db.lock().await;
    let result = pangolin_core::db::upsert_tun(&db, &tun);
    drop(db);

    match result {
        Ok(()) => Ok(redirect_response("/tun")),
        Err(e) => render_edit_page_with_error(&name, &format!("Database error: {}", e), csrf, lang),
    }
}

// NOTE: `handle_delete` is the legacy form-POST endpoint. It will be removed
// once all admin UIs migrate to the HTMX `hx-delete` button (see
// `api_handle_delete` below and the `templates/components/_hx_delete_button.html`
// partial). Tracked by issue #48.
pub async fn handle_delete(
    app: &Arc<App>,
    name: Option<String>,
    _csrf: &str,
    _lang: Lang,
) -> http::Result<Resp> {
    if let Some(n) = name
        && !n.is_empty()
    {
        let db = app.db.lock().await;
        let _ = pangolin_core::db::delete_tun(&db, &n);
        drop(db);
    }
    Ok(redirect_response("/tun"))
}

/// HTMX `DELETE /api/tun/{name}` — returns an empty 200 body so HTMX
/// (with `hx-swap="delete"`) can drop the row without a full page reload.
///
/// This is the unified delete endpoint for tun rows; the form-POST
/// `/tun/delete` route above is kept for now as a fallback during the
/// migration window (issue #48).
pub async fn api_handle_delete(
    app: &Arc<App>,
    name: String,
    _csrf: &str,
    lang: Lang,
) -> http::Result<Resp> {
    if name.is_empty() {
        return Ok(crate::not_found(lang));
    }
    let db = app.db.lock().await;
    let _ = pangolin_core::db::delete_tun(&db, &name);
    drop(db);
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::new()))
        .unwrap())
}
