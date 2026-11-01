//! Sites POST / PUT / DELETE handlers.
//!
//! On validation failure each handler rebuilds a prefill `Site` from the
//! submitted form and re-renders the page with an inline error via the
//! helpers in `pages.rs`. On success the response is a 302 to `/sites`.

use std::sync::Arc;

use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use crate::App;
use crate::redirect_response;
use crate::routes::provider::{guard_can_be_enabled, parse_site_provider};
use crate::routes::ratelimit::{CUSTOM_403_FIELD, parse_policy};
use pangolin_core::types::{HostMode, Site};

use super::helpers::assemble_backend_from_form;
use super::pages::{render_create_page_with_error, render_edit_page_with_error};
use crate::routes::helpers::parse_form;

type Resp = Response<Full<Bytes>>;

pub async fn handle_create(app: &Arc<App>, body: &[u8], csrf: &str) -> http::Result<Resp> {
    let params = parse_form(body);
    let name = params.get("name").cloned().unwrap_or_default();
    // The hidden `backend` field is filled in by the page's JS from the
    // visible form fields. If JS didn't fire (e.g. browser quirk, the
    // user pasted a value that didn't trigger an input event, or the
    // page was re-rendered mid-typing), the hidden can be empty even
    // though the user did fill in host:port. As a safety net, assemble
    // the backend server-side from the individual form fields when
    // `backend` is empty.
    let backend_from_hidden = params.get("backend").cloned().unwrap_or_default();
    let backend = if backend_from_hidden.is_empty() {
        assemble_backend_from_form(&params)
    } else {
        backend_from_hidden
    };
    let host_mode = params
        .get("host_mode")
        .and_then(|v| v.parse::<HostMode>().ok())
        .unwrap_or(HostMode::Passthrough);
    let host_custom = params.get("host_custom").cloned().filter(|v| !v.is_empty());
    // Issue #117: the per-site rate-limit policy. A form without any
    // rate-limit field (an API client that predates the feature)
    // leaves the site without a policy row — the default (limits off)
    // applies.
    let parsed = parse_policy(&params);
    // Issue #122: the provider fields (provider, custom header
    // names, the guard tri-state and the manual ranges).
    let parsed_provider = parse_site_provider(&params);
    // The fields that parsed are kept, so the operator only has to
    // fix the one that was wrong.
    if let Some(e) = &parsed_provider.error {
        return render_create_page_with_error(
            app,
            Some(prefill_site(
                &name,
                &backend,
                host_mode,
                host_custom.clone(),
            )),
            parsed.policy,
            e,
            Some("provider"),
            csrf,
            Some(&parsed_provider),
        )
        .await;
    }
    if let Some(e) = &parsed.error {
        return render_create_page_with_error(
            app,
            Some(prefill_site(
                &name,
                &backend,
                host_mode,
                host_custom.clone(),
            )),
            parsed.policy,
            e,
            Some("rate_limit"),
            csrf,
            Some(&parsed_provider),
        )
        .await;
    }
    if let Err(e) = guard_check(app, &parsed_provider).await {
        return render_create_page_with_error(
            app,
            Some(prefill_site(
                &name,
                &backend,
                host_mode,
                host_custom.clone(),
            )),
            parsed.policy,
            &e,
            Some("provider"),
            csrf,
            Some(&parsed_provider),
        )
        .await;
    }
    let rate_limit = parsed.present.then_some(parsed.policy);

    // Build a prefill Site from the submitted form values. This is what
    // gets passed back to the template on error so the user doesn't lose
    // their input (name, protocol/host/tunnel selections, host_mode,
    // host_custom) when the server rejects the submission.
    let prefill = || prefill_site(&name, &backend, host_mode, host_custom.clone());

    if name.is_empty() {
        return render_create_page_with_error(
            app,
            Some(prefill()),
            rate_limit.unwrap_or_default(),
            "Site name is required",
            Some("name"),
            csrf,
            Some(&parsed_provider),
        )
        .await;
    }
    if backend.is_empty() {
        return render_create_page_with_error(
            app,
            Some(prefill()),
            rate_limit.unwrap_or_default(),
            "Backend is required — fill in the host:port (or file path) field",
            Some("backend"),
            csrf,
            Some(&parsed_provider),
        )
        .await;
    }
    if let Err(e) = pangolin_core::parse::parse_backend(&backend) {
        return render_create_page_with_error(
            app,
            Some(prefill()),
            rate_limit.unwrap_or_default(),
            &format!("Invalid backend: {}", e),
            Some("backend"),
            csrf,
            Some(&parsed_provider),
        )
        .await;
    }

    let site = prefill();

    let db = app.db.lock().await;
    let result = pangolin_core::db::upsert_site(&db, &site);
    // The policy is only written when the site itself was written —
    // a failed site write must not leave an orphaned policy row.
    let limit_result = match &result {
        Ok(()) => rate_limit
            .as_ref()
            .map(|p| pangolin_core::db::upsert_site_rate_limit(&db, &name, p)),
        Err(_) => None,
    };
    let provider_result = match &result {
        Ok(()) => save_provider(&db, &name, &parsed_provider),
        Err(_) => None,
    };
    drop(db);

    match result {
        Ok(()) => {
            if let Some(Err(e)) = limit_result {
                log::warn!("rate limit policy for {name} not saved: {e}");
            }
            if let Some(Err(e)) = provider_result {
                log::warn!("provider configuration for {name} not saved: {e}");
            }
            app.reload_indexes().await;
            Ok(redirect_response("/sites"))
        }
        Err(e) => {
            render_create_page_with_error(
                app,
                Some(prefill()),
                rate_limit.unwrap_or_default(),
                &format!("Database error: {}", e),
                None,
                csrf,
                Some(&parsed_provider),
            )
            .await
        }
    }
}

pub async fn handle_update(
    app: &Arc<App>,
    name: Option<String>,
    body: &[u8],
    csrf: &str,
) -> http::Result<Resp> {
    let name = match name {
        Some(n) if !n.is_empty() => n,
        _ => {
            let mut resp = Response::new(Full::new(Bytes::from(
                r#"<div class="p-6 max-w-md mx-auto"><div class="bg-red-50 border border-red-200 rounded-lg p-4"><h2 class="text-red-800 font-semibold mb-1">Bad request</h2><p class="text-red-700 text-sm">Missing site name.</p><a href="/sites" class="text-sm text-red-700 underline mt-2 inline-block">← Back to sites</a></div></div>"#,
            )));
            *resp.status_mut() = http::StatusCode::BAD_REQUEST;
            return Ok(resp);
        }
    };
    let params = parse_form(body);
    let backend_from_hidden = params.get("backend").cloned().unwrap_or_default();
    let assembled = assemble_backend_from_form(&params);
    let backend = if !backend_from_hidden.is_empty() {
        backend_from_hidden
    } else {
        assembled.clone()
    };
    let host_mode = params
        .get("host_mode")
        .and_then(|v| v.parse::<HostMode>().ok())
        .unwrap_or(HostMode::Passthrough);
    let host_custom = params.get("host_custom").cloned().filter(|v| !v.is_empty());
    // Issue #117: the per-site rate-limit policy. A form without any
    // rate-limit field leaves the existing policy (if any) untouched.
    let parsed = parse_policy(&params);
    // Issue #122: the provider fields (provider, custom header
    // names, the guard tri-state and the manual ranges).
    let parsed_provider = parse_site_provider(&params);
    // The fields that parsed are kept, so the operator only has to
    // fix the one that was wrong.
    if let Some(e) = &parsed_provider.error {
        return render_edit_page_with_error(
            app,
            prefill_site(&name, &backend, host_mode, host_custom.clone()),
            parsed.policy,
            e,
            Some("provider"),
            csrf,
            Some(&parsed_provider),
        )
        .await;
    }
    if let Some(e) = &parsed.error {
        return render_edit_page_with_error(
            app,
            prefill_site(&name, &backend, host_mode, host_custom.clone()),
            parsed.policy,
            e,
            Some("rate_limit"),
            csrf,
            Some(&parsed_provider),
        )
        .await;
    }
    if let Err(e) = guard_check(app, &parsed_provider).await {
        return render_edit_page_with_error(
            app,
            prefill_site(&name, &backend, host_mode, host_custom.clone()),
            parsed.policy,
            &e,
            Some("provider"),
            csrf,
            Some(&parsed_provider),
        )
        .await;
    }
    let rate_limit = parsed.present.then_some(parsed.policy);

    // A form that carries the limit knobs but not the custom-page
    // textarea (an API client that only knows the numeric knobs) must
    // not wipe the stored page.
    let rate_limit = match rate_limit {
        Some(mut policy) => {
            let db = app.db.lock().await;
            let stored = pangolin_core::db::get_site_rate_limit(&db, &name)
                .ok()
                .flatten();
            if !params.contains_key(crate::routes::ratelimit::CUSTOM_429_FIELD) {
                policy.custom_429_body = stored.as_ref().and_then(|p| p.custom_429_body.clone());
            }
            if !params.contains_key(CUSTOM_403_FIELD) {
                policy.custom_403_body = stored.as_ref().and_then(|p| p.custom_403_body.clone());
            }
            Some(policy)
        }
        None => None,
    };

    let prefill = || -> Site { prefill_site(&name, &backend, host_mode, host_custom.clone()) };

    // For edit mode, fetch the existing site once so we can fall back to
    // its backend when the submission was empty (better UX than a
    // blanked-out form on the error path).
    let existing_backend: Option<String> = if backend.is_empty() {
        let db = app.db.lock().await;
        pangolin_core::db::get_site(&db, &name)
            .ok()
            .flatten()
            .map(|s| s.backend)
    } else {
        None
    };
    let effective_backend_submitted = backend.clone();
    let backend_for_prefill = existing_backend.unwrap_or(effective_backend_submitted);
    let prefill_with_fallback = || {
        prefill_site(
            &name,
            &backend_for_prefill.clone(),
            host_mode,
            host_custom.clone(),
        )
    };

    if backend.is_empty() {
        return render_edit_page_with_error(
            app,
            prefill_with_fallback(),
            rate_limit.unwrap_or_default(),
            "Backend is required — fill in the host:port (or file path) field",
            Some("backend"),
            csrf,
            Some(&parsed_provider),
        )
        .await;
    }
    if let Err(e) = pangolin_core::parse::parse_backend(&backend) {
        return render_edit_page_with_error(
            app,
            prefill(),
            rate_limit.unwrap_or_default(),
            &format!("Invalid backend: {}", e),
            Some("backend"),
            csrf,
            Some(&parsed_provider),
        )
        .await;
    }

    let site = prefill();

    let db = app.db.lock().await;
    let result = pangolin_core::db::upsert_site(&db, &site);
    // The policy is only written when the site itself was written —
    // a failed site write must not leave an orphaned policy row.
    let limit_result = match &result {
        Ok(()) => rate_limit
            .as_ref()
            .map(|p| pangolin_core::db::upsert_site_rate_limit(&db, &name, p)),
        Err(_) => None,
    };
    let provider_result = match &result {
        Ok(()) => save_provider(&db, &name, &parsed_provider),
        Err(_) => None,
    };
    drop(db);

    match result {
        Ok(()) => {
            if let Some(Err(e)) = limit_result {
                log::warn!("rate limit policy for {name} not saved: {e}");
            }
            if let Some(Err(e)) = provider_result {
                log::warn!("provider configuration for {name} not saved: {e}");
            }
            app.reload_indexes().await;
            Ok(redirect_response("/sites"))
        }
        Err(e) => {
            render_edit_page_with_error(
                app,
                prefill(),
                rate_limit.unwrap_or_default(),
                &format!("Database error: {}", e),
                None,
                csrf,
                Some(&parsed_provider),
            )
            .await
        }
    }
}

/// Check that the submitted origin-guard setting is usable.
///
/// The guard needs an allowlist to work with: the provider's
/// published ranges or the site's manual ones.
async fn guard_check(
    app: &Arc<App>,
    parsed: &crate::routes::provider::ParsedProvider,
) -> Result<(), String> {
    if !parsed.present {
        return Ok(());
    }
    let guard_on = parsed
        .over
        .as_ref()
        .and_then(|o| o.origin_guard_enabled)
        .unwrap_or(false);
    if !guard_on {
        return Ok(());
    }
    // The **effective** provider decides whether the guard has
    // anything to work with: a site that inherits the global
    // `cloudflare` provider inherits its published ranges too.
    let global = app.system_config.read().await.provider;
    let provider = parsed
        .over
        .as_ref()
        .and_then(|o| o.provider)
        .unwrap_or(global);
    guard_can_be_enabled(provider, &parsed.manual_ranges)
}

/// Persist the provider configuration of a site.
///
/// A site that overrides nothing at all has its row removed, so it
/// keeps following the global defaults. A form that carried no
/// provider field is left alone entirely — an API client that
/// predates the feature must not wipe the operator's configuration.
fn save_provider(
    db: &rusqlite::Connection,
    site_name: &str,
    parsed: &crate::routes::provider::ParsedProvider,
) -> Option<rusqlite::Result<()>> {
    if !parsed.present {
        return None;
    }
    let result = match &parsed.over {
        Some(over) => pangolin_core::db::upsert_site_provider(db, site_name, over),
        None => pangolin_core::db::delete_site_provider(db, site_name).map(|_| ()),
    };
    let result = match result {
        Ok(()) => {
            // The manual ranges are the whole set of the site, so
            // replacing them is right — but only when the form
            // actually carried the field. An API client that posts
            // only the provider must not wipe the saved ranges (for
            // a `custom_lb` site they are the only thing the guard
            // has).
            if parsed.origin_allow_present {
                pangolin_core::db::replace_site_origin_allow(db, site_name, &parsed.manual_ranges)
            } else {
                Ok(())
            }
        }
        Err(e) => Err(e),
    };
    Some(result)
}

/// Rebuild the `Site` shown back to the operator when a submission is
/// rejected, so they don't lose their input.
fn prefill_site(
    name: &str,
    backend: &str,
    host_mode: HostMode,
    host_custom: Option<String>,
) -> Site {
    Site {
        name: name.to_string(),
        backend: backend.to_string(),
        enabled: true,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        host_mode,
        host_custom,
        domain_count: 0,
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
) -> http::Result<Resp> {
    if let Some(n) = name
        && !n.is_empty()
    {
        let db = app.db.lock().await;
        let _ = pangolin_core::db::delete_site(&db, &n);
        drop(db);
        app.reload_indexes().await;
    }
    Ok(redirect_response("/sites"))
}

/// HTMX `DELETE /api/sites/{name}` — returns an empty 200 body so HTMX
/// (with `hx-swap="delete"`) can drop the row without a full page reload.
///
/// This is the unified delete endpoint for sites; the form-POST
/// `/sites/delete` route above is kept for now as a fallback during the
/// migration window (issue #48).
pub async fn api_handle_delete(app: &Arc<App>, name: String, _csrf: &str) -> http::Result<Resp> {
    if name.is_empty() {
        return Ok(crate::not_found());
    }
    let db = app.db.lock().await;
    let _ = pangolin_core::db::delete_site(&db, &name);
    drop(db);
    app.reload_indexes().await;
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::new()))
        .unwrap())
}
