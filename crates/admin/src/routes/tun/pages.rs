//! Tunnels full-page renders.

use askama::Template;
use std::sync::Arc;

use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use crate::App;
use crate::i18n::Lang;
use crate::routes::helpers::bad_request_page;
use crate::templates::{TunnelsEditTemplate, TunnelsListTemplate, TunnelsNewTemplate};

type Resp = Response<Full<Bytes>>;

fn ok_html(body: String) -> http::Result<Resp> {
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .unwrap())
}

pub async fn render(app: &Arc<App>, csrf: &str, lang: Lang) -> http::Result<Resp> {
    let db = app.db.lock().await;
    let tuns = pangolin_core::db::list_tuns(&db).unwrap_or_default();
    drop(db);
    let html = TunnelsListTemplate {
        tuns,
        active_nav: "tun",
        lang,
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

pub async fn render_create_page(csrf: &str, lang: Lang) -> http::Result<Resp> {
    let html = TunnelsNewTemplate {
        tun: None,
        action: "create",
        error: None,
        active_nav: "tun",
        lang,
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

pub async fn render_edit_page(
    app: &Arc<App>,
    name: Option<String>,
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

    let db = app.db.lock().await;
    let tun = pangolin_core::db::get_tun(&db, &name).unwrap_or_default();
    drop(db);

    if tun.is_none() {
        return render_edit_page_with_error(&name, &lang.t("tunnels.error.not_found"), csrf, lang);
    }

    let html = TunnelsEditTemplate {
        tun,
        action: "update",
        error: None,
        active_nav: "tun",
        lang,
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

pub(super) fn render_create_page_with_error(
    tun: Option<pangolin_core::types::Tun>,
    error: &str,
    csrf: &str,
    lang: Lang,
) -> http::Result<Resp> {
    let html = TunnelsNewTemplate {
        tun,
        action: "create",
        error: Some(error),
        active_nav: "tun",
        lang,
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

pub(super) fn render_edit_page_with_error(
    name: &str,
    error: &str,
    csrf: &str,
    lang: Lang,
) -> http::Result<Resp> {
    let stub = pangolin_core::types::Tun {
        name: name.to_string(),
        token: None,
        token_hash: None,
        enabled: true,
        online: false,
        registered_at: None,
        last_seen_at: None,
        expires_at: None,
    };
    let html = TunnelsEditTemplate {
        tun: Some(stub),
        action: "update",
        error: Some(error),
        active_nav: "tun",
        lang,
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}
