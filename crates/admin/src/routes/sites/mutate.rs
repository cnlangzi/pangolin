//! Sites POST / PUT / DELETE handlers.
//!
//! On validation failure each handler rebuilds a prefill `Site` from the
//! submitted form and re-renders the page with an inline error via the
//! helpers in `pages.rs`. On success the response is a 302 to `/sites`.

use std::collections::BTreeSet;
use std::sync::Arc;

use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use crate::App;
use crate::redirect_response;
use crate::routes::ratelimit::{merge_bot_list, renderable_bot_ids};
use pangolin_core::types::Site;

use super::helpers::{parse_site_form, site_prefill};
use super::pages::{FormContext, render_create_page_with_error, render_edit_page_with_error};
use crate::routes::helpers::parse_form;

type Resp = Response<Full<Bytes>>;

pub async fn handle_create(app: &Arc<App>, body: &[u8], csrf: &str) -> http::Result<Resp> {
    let params = parse_form(body);
    let name = params.get("name").cloned().unwrap_or_default();
    // The provider of the global configuration — a site that overrides
    // nothing inherits it, and the origin guard needs an allowlist.
    let global_provider = app.system_config.read().await.provider;
    // The whole form is validated by one shared parser (issue #185):
    // the JSON API validates through the very same function, so the
    // two surfaces can never drift apart.
    let parsed = match parse_site_form(&params, &name, None, global_provider) {
        Ok(p) => p,
        Err(e) => {
            let submitted_bots = e.rate_limit_bots.clone().unwrap_or_default();
            return render_create_page_with_error(
                app,
                FormContext {
                    site: Some(site_prefill(&params, &name)),
                    rate_limit: e.rate_limit,
                    rate_limit_bots: submitted_bots,
                    provider: Some(&e.provider),
                },
                &e.message,
                e.field,
                csrf,
            )
            .await;
        }
    };
    // The site's stored list, read before the write: the ids the form
    // cannot show have to survive the save (see `merge_bot_list`).
    let stored_bots: BTreeSet<String> = {
        let db = app.db.lock().await;
        pangolin_core::db::get_site_rate_limit_bots(&db, &name).unwrap_or_default()
    };

    let site = Site {
        name: parsed.name.clone(),
        backend: parsed.backend.clone(),
        enabled: true,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        host_mode: parsed.host_mode,
        host_custom: parsed.host_custom.clone(),
        domain_count: 0,
    };

    let db = app.db.lock().await;
    let result = pangolin_core::db::upsert_site(&db, &site);
    // The policy is only written when the site itself was written —
    // a failed site write must not leave an orphaned policy row.
    let limit_result = match &result {
        Ok(()) => parsed
            .rate_limit
            .as_ref()
            .map(|p| pangolin_core::db::upsert_site_rate_limit(&db, &name, p)),
        Err(_) => None,
    };
    let provider_result = match &result {
        Ok(()) => save_provider(&db, &name, &parsed.provider),
        Err(_) => None,
    };
    let bots_result = save_bot_list(
        &db,
        app,
        &name,
        &result,
        parsed.rate_limit_bots.as_ref(),
        &stored_bots,
    );
    drop(db);

    match result {
        Ok(()) => {
            if let Some(Err(e)) = limit_result {
                log::warn!("rate limit policy for {name} not saved: {e}");
            }
            if let Some(Err(e)) = provider_result {
                log::warn!("provider configuration for {name} not saved: {e}");
            }
            if let Some(Err(e)) = bots_result {
                log::warn!("rate limit bot list for {name} not saved: {e}");
            }
            app.reload_indexes().await;
            Ok(redirect_response("/sites"))
        }
        Err(e) => {
            render_create_page_with_error(
                app,
                FormContext {
                    site: Some(site_prefill(&params, &name)),
                    rate_limit: parsed.rate_limit.clone().unwrap_or_default(),
                    rate_limit_bots: parsed.rate_limit_bots.clone().unwrap_or_default(),
                    provider: Some(&parsed.provider),
                },
                &format!("Database error: {}", e),
                None,
                csrf,
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

    // The site's stored state, read before the write: the rate-limit
    // policy supplies the custom error pages the submission did not
    // mention, and the bot list supplies the ids the form cannot show
    // (see `merge_bot_list`).
    let (stored_policy, stored_bots) = {
        let db = app.db.lock().await;
        let policy = pangolin_core::db::get_site_rate_limit(&db, &name)
            .ok()
            .flatten();
        let bots = pangolin_core::db::get_site_rate_limit_bots(&db, &name).unwrap_or_default();
        (policy, bots)
    };
    // For edit mode, fetch the existing site once so we can fall back to
    // its backend when the submission was empty (better UX than a
    // blanked-out form on the error path).
    let existing_backend: Option<String> = {
        let db = app.db.lock().await;
        pangolin_core::db::get_site(&db, &name)
            .ok()
            .flatten()
            .map(|s| s.backend)
    };
    let global_provider = app.system_config.read().await.provider;
    let parsed = match parse_site_form(&params, &name, stored_policy.as_ref(), global_provider) {
        Ok(p) => p,
        Err(e) => {
            // The bot ids the re-rendered form shows as checked: the
            // submitted ones, or the stored ones when the submission
            // carried no list.
            let submitted_bots = e
                .rate_limit_bots
                .clone()
                .unwrap_or_else(|| stored_bots.clone());
            let mut prefill = site_prefill(&params, &name);
            if prefill.backend.is_empty()
                && let Some(backend) = existing_backend.clone()
            {
                prefill.backend = backend;
            }
            return render_edit_page_with_error(
                app,
                FormContext {
                    site: Some(prefill),
                    rate_limit: e.rate_limit,
                    rate_limit_bots: submitted_bots,
                    provider: Some(&e.provider),
                },
                &e.message,
                e.field,
                csrf,
            )
            .await;
        }
    };

    let site = Site {
        name: parsed.name.clone(),
        backend: parsed.backend.clone(),
        enabled: true,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        host_mode: parsed.host_mode,
        host_custom: parsed.host_custom.clone(),
        domain_count: 0,
    };

    let db = app.db.lock().await;
    let result = pangolin_core::db::upsert_site(&db, &site);
    // The policy is only written when the site itself was written —
    // a failed site write must not leave an orphaned policy row.
    let limit_result = match &result {
        Ok(()) => parsed
            .rate_limit
            .as_ref()
            .map(|p| pangolin_core::db::upsert_site_rate_limit(&db, &name, p)),
        Err(_) => None,
    };
    let provider_result = match &result {
        Ok(()) => save_provider(&db, &name, &parsed.provider),
        Err(_) => None,
    };
    let bots_result = save_bot_list(
        &db,
        app,
        &name,
        &result,
        parsed.rate_limit_bots.as_ref(),
        &stored_bots,
    );
    drop(db);

    match result {
        Ok(()) => {
            if let Some(Err(e)) = limit_result {
                log::warn!("rate limit policy for {name} not saved: {e}");
            }
            if let Some(Err(e)) = provider_result {
                log::warn!("provider configuration for {name} not saved: {e}");
            }
            if let Some(Err(e)) = bots_result {
                log::warn!("rate limit bot list for {name} not saved: {e}");
            }
            app.reload_indexes().await;
            Ok(redirect_response("/sites"))
        }
        Err(e) => {
            let mut prefill = site_prefill(&params, &name);
            if prefill.backend.is_empty()
                && let Some(backend) = existing_backend
            {
                prefill.backend = backend;
            }
            render_edit_page_with_error(
                app,
                FormContext {
                    site: Some(prefill),
                    rate_limit: parsed.rate_limit.clone().unwrap_or_default(),
                    rate_limit_bots: parsed.rate_limit_bots.clone().unwrap_or_default(),
                    provider: Some(&parsed.provider),
                },
                &format!("Database error: {}", e),
                None,
                csrf,
            )
            .await
        }
    }
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

/// Persist the site's bot-limited list (issue #131).
///
/// The list is written only after the site row itself was written —
/// a failed site write must not leave an orphaned list. A submission
/// that carried no list at all leaves the stored list untouched.
///
/// The stored ids the form cannot show survive the save (see
/// [`merge_bot_list`]): the operator has no way to manage them
/// through the form, so an unrelated save must not silently drop
/// them.
fn save_bot_list(
    db: &rusqlite::Connection,
    app: &Arc<App>,
    site_name: &str,
    site_result: &rusqlite::Result<()>,
    submitted: Option<&BTreeSet<String>>,
    stored: &BTreeSet<String>,
) -> Option<rusqlite::Result<()>> {
    if site_result.is_err() {
        return None;
    }
    let submitted = submitted?;
    let bots = merge_bot_list(submitted, stored, &renderable_bot_ids(app));
    Some(pangolin_core::db::set_site_rate_limit_bots(
        db, site_name, &bots,
    ))
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

#[cfg(test)]
mod tests {
    use super::*;
    use pangolin_core::CertManager;
    use pangolin_core::config::Config;
    use std::collections::BTreeSet;
    use tempfile::TempDir;

    fn make_test_app() -> (TempDir, Arc<App>) {
        let dir = TempDir::new().expect("tempdir");
        let db_path = dir.path().join("test.db");
        let app = Arc::new(
            App::new(
                db_path.to_str().expect("utf8"),
                Config::default(),
                CertManager::default(),
            )
            .expect("App::new"),
        );
        (dir, app)
    }

    async fn html_of(resp: Response<Full<Bytes>>) -> String {
        let bytes = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .expect("body")
            .to_bytes();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn bots(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    /// The site's list is written next to the policy — the form's
    /// checked boxes become the site's list (issue #131).
    #[tokio::test]
    async fn the_checked_boxes_are_saved_as_the_site_list() {
        let (_dir, app) = make_test_app();
        let resp = handle_create(
            &app,
            b"name=a&backend=http://127.0.0.1:8080&rate_limit_bots_applied=1&rate_limit_bot_gptbot=on&rate_limit_bot_ccbot=on",
            "csrf",
        )
        .await
        .unwrap();
        assert_eq!(resp.status().as_u16(), 302);

        let db = app.db.lock().await;
        let stored = pangolin_core::db::get_site_rate_limit_bots(&db, "a").unwrap();
        assert_eq!(stored, bots(&["gptbot", "ccbot"]));
    }

    /// A submission that carries no list at all leaves the stored list
    /// untouched — an API client that predates the feature must not
    /// wipe it.
    #[tokio::test]
    async fn a_submission_without_the_list_keeps_the_stored_list() {
        let (_dir, app) = make_test_app();
        handle_create(
            &app,
            b"name=a&backend=http://127.0.0.1:8080&rate_limit_bots_applied=1&rate_limit_bot_gptbot=on",
            "csrf",
        )
        .await
        .unwrap();

        let resp = handle_update(
            &app,
            Some("a".to_string()),
            b"name=a&backend=http://127.0.0.1:8080&rate_limit_req_limit=10",
            "csrf",
        )
        .await
        .unwrap();
        assert_eq!(resp.status().as_u16(), 302);

        let db = app.db.lock().await;
        let stored = pangolin_core::db::get_site_rate_limit_bots(&db, "a").unwrap();
        assert_eq!(stored, bots(&["gptbot"]));
    }

    /// Un-checking every box empties the list — every verified bot is
    /// exempt on that site again.
    #[tokio::test]
    async fn unchecking_every_box_empties_the_list() {
        let (_dir, app) = make_test_app();
        handle_create(
            &app,
            b"name=a&backend=http://127.0.0.1:8080&rate_limit_bots_applied=1&rate_limit_bot_gptbot=on",
            "csrf",
        )
        .await
        .unwrap();

        let resp = handle_update(
            &app,
            Some("a".to_string()),
            b"name=a&backend=http://127.0.0.1:8080&rate_limit_bots_applied=1",
            "csrf",
        )
        .await
        .unwrap();
        assert_eq!(resp.status().as_u16(), 302);

        let db = app.db.lock().await;
        let stored = pangolin_core::db::get_site_rate_limit_bots(&db, "a").unwrap();
        assert!(stored.is_empty());
    }

    /// Whether the checkbox of `bot` is checked in the rendered HTML.
    ///
    /// The checkbox is rendered as one `<input>` tag; the `checked`
    /// attribute sits inside it, so the tag is what has to be looked
    /// at — the page carries many other checkboxes.
    fn checkbox_is_checked(body: &str, bot: &str) -> bool {
        let needle = format!("name=\"rate_limit_bot_{bot}\"");
        let Some(start) = body.find(&needle) else {
            return false;
        };
        let rest = &body[start..];
        let end = rest.find('>').map(|i| i + 1).unwrap_or(rest.len());
        rest[..end].contains("checked")
    }

    /// A stored id the form cannot show survives a save: the operator
    /// has no way to manage it through the form, so an unrelated save
    /// must not silently drop it (issue #131).
    #[tokio::test]
    async fn a_stored_id_the_form_cannot_show_survives_a_save() {
        let (_dir, app) = make_test_app();
        handle_create(
            &app,
            b"name=a&backend=http://127.0.0.1:8080&rate_limit_bots_applied=1&rate_limit_bot_gptbot=on",
            "csrf",
        )
        .await
        .unwrap();

        // A bot whose registry id is not safe as a form field name is
        // never rendered as a checkbox — it can only be in the table
        // through the API or by hand.
        {
            let db = app.db.lock().await;
            pangolin_core::db::set_site_rate_limit_bots(
                &db,
                "a",
                &["gptbot".to_string(), "my.bot".to_string()]
                    .into_iter()
                    .collect(),
            )
            .unwrap();
        }

        // The operator saves the site with no box checked.
        let resp = handle_update(
            &app,
            Some("a".to_string()),
            b"name=a&backend=http://127.0.0.1:8080&rate_limit_bots_applied=1",
            "csrf",
        )
        .await
        .unwrap();
        assert_eq!(resp.status().as_u16(), 302);

        let db = app.db.lock().await;
        let stored = pangolin_core::db::get_site_rate_limit_bots(&db, "a").unwrap();
        assert_eq!(
            stored,
            ["my.bot".to_string()]
                .into_iter()
                .collect::<BTreeSet<String>>(),
            "the renderable bot follows the form, the un-renderable one survives"
        );
    }

    /// The site form renders one checkbox per known bot, with the
    /// site's stored list pre-checked (issue #131, R8).
    #[tokio::test]
    async fn the_edit_page_renders_a_checkbox_per_known_bot() {
        let (_dir, app) = make_test_app();
        handle_create(
            &app,
            b"name=a&backend=http://127.0.0.1:8080&rate_limit_bots_applied=1&rate_limit_bot_gptbot=on",
            "csrf",
        )
        .await
        .unwrap();

        let body = html_of(
            super::super::pages::render_edit_page(&app, Some("a".to_string()), "csrf")
                .await
                .unwrap(),
        )
        .await;
        // The marker that tells the server the submission carries the
        // list at all.
        assert!(
            body.contains("name=\"rate_limit_bots_applied\""),
            "the form must carry the marker field"
        );
        // One checkbox per known bot.
        for bot in ["googlebot", "gptbot", "ccbot"] {
            assert!(
                body.contains(&format!("name=\"rate_limit_bot_{bot}\"")),
                "the form must offer a checkbox for {bot}"
            );
        }
        // The stored list is pre-checked.
        assert!(
            checkbox_is_checked(&body, "gptbot"),
            "the site's stored list must be pre-checked"
        );
        // Every bot carries its category badge — the same one the
        // bot log shows, so the operator recognises an AI scraper at a
        // glance.
        assert!(
            body.contains("bg-purple-100") && body.contains("ai_bot"),
            "the form must badge each bot with its category"
        );
        assert!(
            !checkbox_is_checked(&body, "googlebot"),
            "a bot that is not in the site's list must not be pre-checked"
        );
    }
}
