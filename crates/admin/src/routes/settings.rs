//! Settings page — `/settings` (GET + POST).
//!
//! Operator-facing HTML form for the system_config singleton
//! (fix-client-ip). Renders on GET; on POST validates via
//! [`super::system::parse_system_config_form`] (shared with the
//! JSON API at `/api/system/config`), persists via
//! [`super::system::save_system_config`], and either 302s back to
//! `/settings` on success or re-renders the form with an inline
//! error on failure (so the operator can fix the input without
//! losing what they typed).
//!
//! CSRF is enforced globally in `lib.rs` for every POST.

use std::sync::Arc;

use askama::Template;
use bytes::Bytes;
use http::{Response, StatusCode};
use http_body_util::Full;
use pangolin_core::{App, SystemConfig};

use super::helpers::ok_html;
use super::system::{parse_system_config_form, save_system_config};
use crate::render_with_assets_and_csrf;
use crate::templates::SettingsTemplate;

/// Render the settings page. `error` is `None` on the initial
/// GET and on a successful POST (which 302s back here); it's
/// `Some(msg)` when re-rendering after a failed POST.
async fn render_page(
    app: &Arc<App>,
    csrf: &str,
    error: Option<&str>,
) -> http::Result<Response<Full<Bytes>>> {
    let cfg = app.system_config.read().await.clone();
    let template = SettingsTemplate {
        frontend_mode: match cfg.frontend_mode {
            pangolin_core::FrontendMode::Direct => "direct",
            pangolin_core::FrontendMode::Cloudflare => "cloudflare",
            pangolin_core::FrontendMode::CustomLb => "custom_lb",
        },
        // Display the current value as JSON in the textarea so the
        // operator sees what they have. Empty list renders as `[]`
        // (a valid JSON array) rather than blank.
        trusted_headers_json: render_trusted_headers(&cfg.trusted_headers),
        updated_at: cfg.updated_at.to_rfc3339(),
        error,
        active_nav: "settings",
    };
    ok_html(render_with_assets_and_csrf(
        template.render().unwrap(),
        csrf,
    ))
}

/// `GET /settings` — render the form with the current values.
pub async fn handle_render_settings(
    app: &Arc<App>,
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    render_page(app, csrf, None).await
}

/// `POST /settings` — validate, persist, redirect; or re-render
/// with the error message inline.
pub async fn handle_update_settings(
    app: &Arc<App>,
    csrf: &str,
    body: &[u8],
) -> http::Result<Response<Full<Bytes>>> {
    match parse_system_config_form(body) {
        Ok(parsed) => {
            // Take ownership so we can move into save_system_config
            // without re-cloning the SystemConfig.
            let cfg: SystemConfig = parsed;
            if let Err(msg) = save_system_config(app, cfg).await {
                return render_page(app, csrf, Some(&msg)).await;
            }
            // Success: PRG (Post-Redirect-Get) so a refresh
            // doesn't re-submit the form.
            Ok(Response::builder()
                .status(StatusCode::FOUND)
                .header(http::header::LOCATION, "/settings")
                .body(Full::new(Bytes::new()))
                .expect("302 redirect response builder should not fail"))
        }
        Err(msg) => render_page(app, csrf, Some(&msg)).await,
    }
}

/// Render `headers` as the textarea default value.
///
/// * `[]` (empty) → `"[]"` so the textarea is never blank and
///   remains valid JSON.
fn render_trusted_headers(headers: &[String]) -> String {
    serde_json::to_string(headers).unwrap_or_else(|_| "[]".to_string())
}
