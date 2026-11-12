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
use super::provider::{provider_range_statuses, total_guard_rejections};
use super::system::{parse_system_config_form_merged, save_system_config};
use crate::render_with_assets_and_csrf;
use crate::templates::SettingsTemplate;
use crate::templates::settings::{BadbotsSettingsView, ProviderOption};

/// Render the settings page. `error` is `None` on the initial
/// GET and on a successful POST (which 302s back here); it's
/// `Some(msg)` when re-rendering after a failed POST.
async fn render_page(
    app: &Arc<App>,
    csrf: &str,
    error: Option<&str>,
) -> http::Result<Response<Full<Bytes>>> {
    let cfg = app.system_config.read().await.clone();
    // The provider radio group is generated from the registry, so a
    // provider added to it appears without touching the template.
    let providers = pangolin_core::ProviderKey::all()
        .iter()
        .map(|key| {
            let spec = pangolin_core::provider::spec(*key);
            ProviderOption {
                key: spec.key.as_str(),
                label: spec.label,
                description: spec.description,
            }
        })
        .collect();
    // The IP reputation settings (issue #141) live in the database.
    // A read failure must not take the whole settings page down — the
    // card falls back to the documented defaults.
    let badbots = match app.badbots_settings().await {
        Ok(s) => BadbotsSettingsView {
            enabled: s.enabled,
            evidence_policy: s.evidence_policy.as_str(),
            feed_refresh_interval_secs: s.feed_refresh_interval_secs,
            abuseipdb_key_configured: s.abuseipdb_api_key.is_some(),
            crowdsec_key_configured: s.crowdsec_api_key.is_some(),
            updated_at: s.updated_at.to_rfc3339(),
        },
        Err(e) => {
            log::warn!("settings: reading the badbots configuration failed: {e}");
            BadbotsSettingsView {
                enabled: true,
                evidence_policy: "lenient",
                feed_refresh_interval_secs: 300,
                abuseipdb_key_configured: false,
                crowdsec_key_configured: false,
                updated_at: String::new(),
            }
        }
    };
    let template = SettingsTemplate {
        provider: cfg.provider.as_str(),
        providers,
        pgl_headers_enabled: cfg.pgl_headers_enabled,
        // The header keys are rendered as plain text inputs; an
        // unset key renders as an empty field ("not configured").
        client_ip_header: cfg.client_ip_header.as_deref().unwrap_or(""),
        client_country_header: cfg.client_country_header.as_deref().unwrap_or(""),
        client_region_header: cfg.client_region_header.as_deref().unwrap_or(""),
        origin_guard_enabled: cfg.origin_guard_enabled,
        provider_ranges: provider_range_statuses(app).await,
        guard_rejections: total_guard_rejections(app),
        updated_at: cfg.updated_at.to_rfc3339(),
        badbots,
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
    match parse_system_config_form_merged(app, body).await {
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
