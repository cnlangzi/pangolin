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
use crate::templates::settings::{BadbotsSettingsView, DashboardApiView, ProviderOption};

/// Render the settings page. `error` is `None` on the initial
/// GET and on a successful POST (which 302s back here); it's
/// `Some(msg)` when re-rendering after a failed POST.
async fn render_page(
    app: &Arc<App>,
    csrf: &str,
    error: Option<&str>,
    dashboard_error: Option<&str>,
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
            abuseipdb_key_configured: s.abuseipdb_api_key.is_some(),
            crowdsec_key_configured: s.crowdsec_api_key.is_some(),
            updated_at: s.updated_at.to_rfc3339(),
        },
        Err(e) => {
            log::warn!("settings: reading the badbots configuration failed: {e}");
            BadbotsSettingsView {
                enabled: true,
                evidence_policy: "lenient",
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
        dashboard: dashboard_view(app).await,
        error,
        dashboard_error,
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
    render_page(app, csrf, None, None).await
}

/// The dashboard API's connection settings as the page shows them.
///
/// A gateway that has never been set up has no row yet — the card
/// then shows the empty state and the operator has to restart the
/// gateway once (the row is seeded on the first boot).
async fn dashboard_view(app: &Arc<App>) -> DashboardApiView {
    let row = {
        let db = app.db.lock().await;
        pangolin_core::db::get_api_config(&db).unwrap_or(None)
    };
    match row {
        Some(row) => DashboardApiView {
            instance_id: row.instance_id,
            enabled: row.enabled,
            has_token: !row.token_hash.is_empty(),
            dashboard_port: row.dashboard_port.clamp(0, u16::MAX as i64) as u16,
            dashboard_ip: row.dashboard_ip.unwrap_or_default(),
            listen_addr: app.config.api.listen.clone(),
            listen_enabled: listener_enabled(&app.config.api.listen),
        },
        None => DashboardApiView {
            instance_id: String::new(),
            enabled: false,
            has_token: false,
            dashboard_port: 0,
            dashboard_ip: String::new(),
            listen_addr: app.config.api.listen.clone(),
            listen_enabled: listener_enabled(&app.config.api.listen),
        },
    }
}

/// Whether the API's listener runs at all.
///
/// `api.listen: ":0"` switches it off — the card has to say so,
/// because turning the API on then changes nothing.
fn listener_enabled(listen: &str) -> bool {
    !listen.is_empty() && listen != ":0"
}

/// `POST /settings/dashboard` — the dashboard API's connection
/// settings.
///
/// The bearer token is **write-only**: the field is empty on every
/// render, and a submission that leaves it empty keeps the stored
/// token. The node stores only the token's SHA-256 hash, so the
/// token cannot be read back — not by the operator, not by the API.
pub async fn handle_update_dashboard(
    app: &Arc<App>,
    csrf: &str,
    body: &[u8],
) -> http::Result<Response<Full<Bytes>>> {
    let params = super::helpers::parse_form(body);

    let row = {
        let db = app.db.lock().await;
        pangolin_core::db::get_api_config(&db).unwrap_or(None)
    };
    let Some(row) = row else {
        return render_with_dashboard_error(
            app,
            csrf,
            "The dashboard API has not been set up on this gateway yet — restart it once.",
        )
        .await;
    };

    // The switch. The form pairs the checkbox with a hidden "false",
    // so the field is always present and its value decides.
    let enabled = super::helpers::param_bool(body, "dashboard_enabled", false);

    // The token: an empty field keeps the stored one, the "remove"
    // checkbox clears it.
    let token_hash = if super::helpers::param_bool(body, "dashboard_token_clear", false) {
        String::new()
    } else if let Some(token) = params
        .get("dashboard_token")
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
    {
        pangolin_core::db::sha256_hex(token)
    } else {
        row.token_hash.clone()
    };

    // The dashboard's port.
    let dashboard_port = match params
        .get("dashboard_port")
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
    {
        Some(port) => match port.parse::<u16>() {
            Ok(p) => p as i64,
            Err(_) => {
                return render_with_dashboard_error(
                    app,
                    csrf,
                    "The dashboard port must be a number between 0 and 65535.",
                )
                .await;
            }
        },
        None => row.dashboard_port,
    };

    {
        let db = app.db.lock().await;
        // Only the three columns this form owns are written: the
        // instance id and the learned address belong to other writers.
        if let Err(e) =
            pangolin_core::db::update_api_settings(&db, enabled, &token_hash, dashboard_port)
        {
            return render_with_dashboard_error(
                app,
                csrf,
                &format!("Writing the dashboard settings failed: {e}"),
            )
            .await;
        }
    }
    Ok(Response::builder()
        .status(StatusCode::FOUND)
        .header(http::header::LOCATION, "/settings")
        .body(Full::new(Bytes::new()))
        .expect("302 redirect response builder should not fail"))
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
                return render_page(app, csrf, Some(&msg), None).await;
            }
            // Success: PRG (Post-Redirect-Get) so a refresh
            // doesn't re-submit the form.
            Ok(Response::builder()
                .status(StatusCode::FOUND)
                .header(http::header::LOCATION, "/settings")
                .body(Full::new(Bytes::new()))
                .expect("302 redirect response builder should not fail"))
        }
        Err(msg) => render_page(app, csrf, Some(&msg), None).await,
    }
}

/// Render the settings page with an error message inline.
///
/// Used by the IP reputation form (`POST /settings/badbots`), which is
/// a form of its own on the page: a failure re-renders the page with
/// the message where the operator was, instead of a standalone error
/// page.
///
/// The page is rendered from the **stored** row, so the values that
/// were typed but not saved are lost — the operator retypes them.
pub async fn render_with_error(
    app: &Arc<App>,
    csrf: &str,
    error: &str,
) -> http::Result<Response<Full<Bytes>>> {
    render_page(app, csrf, Some(error), None).await
}

/// Render the settings page with an error message in the **dashboard**
/// card.
///
/// The dashboard form has a slot of its own: a message about the
/// bearer token belongs next to the token field, not in a card the
/// operator did not submit.
pub async fn render_with_dashboard_error(
    app: &Arc<App>,
    csrf: &str,
    error: &str,
) -> http::Result<Response<Full<Bytes>>> {
    render_page(app, csrf, None, Some(error)).await
}
