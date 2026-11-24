//! Domains POST / DELETE handlers.
//!
//! Behaviour is preserved byte-for-byte from the pre-refactor
//! `routes/domains.rs` — only file layout changed.

use askama::Template;

use crate::i18n::Lang;
use std::sync::Arc;

use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use crate::routes::helpers::parse_form;
use crate::templates::{DomainsEditTemplate, DomainsNewTemplate};
use crate::{App, redirect_response};

use std::collections::HashMap;

use pangolin_core::types::{ChallengeKind, Domain};

type Resp = Response<Full<Bytes>>;

/// The parsed contents of a domain form.
///
/// Everything the handler needs to write the domain row.
pub struct DomainForm {
    pub site_name: String,
    pub enabled: bool,
    pub auto_issue: bool,
    pub dns_provider: Option<String>,
    pub challenge_kind: Option<ChallengeKind>,
}

/// Parse and validate a domain form.
///
/// The same parser serves the HTML form and the dashboard-facing JSON
/// API (issue #185) — a submission the form rejects is rejected here
/// too, with the same message.
///
/// * `domain` — the domain itself: from the form on create, from the
///   path on update.
/// * `existing` — the stored row on update, `None` on create. It
///   decides the defaults of the fields the submission omits and
///   whether the domain name itself has to be checked.
/// * `dns_provider_exists` — whether a named DNS provider exists; a
///   domain that names one that does not exist is rejected.
pub fn parse_domain_form(
    params: &HashMap<String, String>,
    domain: &str,
    existing: Option<&Domain>,
    dns_provider_exists: &dyn Fn(&str) -> bool,
    lang: Lang,
) -> Result<DomainForm, String> {
    // The domain name is required on create; on update it is the
    // primary key and comes from the path.
    if existing.is_none() && domain.is_empty() {
        return Err(lang.t("domains.error.name_required").into_owned());
    }
    let site_name = match existing {
        Some(e) => params
            .get("site_name")
            .cloned()
            .unwrap_or_else(|| e.site_name.clone()),
        None => params.get("site_name").cloned().unwrap_or_default(),
    };
    if site_name.is_empty() {
        return Err(lang.t("domains.error.select_site").into_owned());
    }
    if existing.is_none() && !pangolin_core::is_valid_domain(domain) {
        return Err(lang.t("domains.error.invalid_format").into_owned());
    }
    let dns_provider = params
        .get("dns_provider")
        .cloned()
        .unwrap_or_default()
        .trim()
        .to_string();
    let dns_provider = if dns_provider.is_empty() {
        None
    } else {
        Some(dns_provider)
    };
    // Per-domain challenge kind (issue #55). The form value is one of
    // `"auto"` (NULL — planner picks), `"http-01"`, `"dns-01"`,
    // `"dns-persist-01"`. Anything else (empty, garbage) is treated
    // as `"auto"` to preserve the legacy behaviour and keep the
    // form forgiving when the dropdown is missing. The mapping
    // is delegated to the canonical `FromStr` impl on
    // `ChallengeKind` so a fourth variant would only need to be
    // added in one place.
    let challenge_kind_raw = params
        .get("challenge_kind")
        .map(String::as_str)
        .unwrap_or("")
        .trim();
    let challenge_kind = challenge_kind_raw.parse::<ChallengeKind>().ok();

    // Wildcard domains must have a DNS association (DNS-01 is the only
    // way to validate `*.example.com`).
    if domain.starts_with("*.") && dns_provider.is_none() {
        return Err(lang.t("domains.error.wildcard_needs_dns").into_owned());
    }
    // Issue #55 / RFC 8555 §8.3: a wildcard SAN cannot be validated
    // with http-01. Reject the combination at save time so the
    // operator sees a clear error before the ACME server refuses.
    // The error message MUST contain the literal "RFC 8555 §8.3" —
    // the planner emits the same string and the admin UI test grep
    // for it.
    if domain.starts_with("*.") && matches!(challenge_kind, Some(ChallengeKind::Http01)) {
        return Err(lang.t("domains.error.wildcard_http01").into_owned());
    }
    // If a DNS provider is referenced, verify it exists.
    if let Some(ref name) = dns_provider
        && !dns_provider_exists(name)
    {
        return Err(lang.tf("domains.error.dns_provider_missing", [("name", name)]));
    }

    // The checkboxes: present ⇒ true, absent ⇒ the stored value (or
    // the documented default on create).
    let enabled = params
        .get("enabled")
        .map(|_| true)
        .unwrap_or_else(|| existing.map(|e| e.enabled).unwrap_or(true));
    let auto_issue = params
        .get("auto_issue")
        .map(|_| true)
        .unwrap_or_else(|| existing.map(|e| e.auto_issue).unwrap_or(false));

    Ok(DomainForm {
        site_name,
        enabled,
        auto_issue,
        dns_provider,
        challenge_kind,
    })
}

fn ok_html(body: String) -> http::Result<Resp> {
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .expect("response builder for 200 OK should not fail"))
}

pub async fn handle_create(
    app: &Arc<App>,
    body: &[u8],
    csrf: &str,
    lang: Lang,
) -> http::Result<Resp> {
    let params = parse_form(body);
    let domain = params.get("domain").cloned().unwrap_or_default();
    // The whole form is validated by one shared parser (issue #185):
    // the JSON API validates through the very same function.
    // The parse reads the DNS providers, so it runs on the blocking
    // pool with the connection in hand (issue #254).
    let domain_for_db = domain.clone();
    let parsed = match app
        .with_db(move |conn| {
            parse_domain_form(
                &params,
                &domain_for_db,
                None,
                &|name| {
                    pangolin_core::db::get_dns_provider(conn, name)
                        .unwrap_or(None)
                        .is_some()
                },
                lang,
            )
        })
        .await
    {
        Ok(parsed) => parsed,
        Err(e) => {
            log::warn!("domains: the form could not be parsed: {e}");
            return render_create_page_with_error(
                app,
                &lang.t("domains.error.form_check_failed"),
                csrf,
                None,
                lang,
            )
            .await;
        }
    };
    let parsed = match parsed {
        Ok(p) => p,
        Err(msg) => return render_create_page_with_error(app, &msg, csrf, None, lang).await,
    };

    let d = pangolin_core::types::Domain {
        domain,
        site_name: parsed.site_name,
        enabled: parsed.enabled,
        auto_issue: parsed.auto_issue,
        dns_provider: parsed.dns_provider,
        challenge_kind: parsed.challenge_kind,
        created_at: chrono::Utc::now(),
    };

    // The write runs on the blocking pool, under one connection: the
    // domain row, its cert placeholder and the link-cache refresh
    // belong to the same write (issue #254).
    let cert_links = app.cert_links.clone();
    let cert_dir = app.cert_manager.cert_dir.clone();
    let result = app
        .with_db_res(move |db| {
            let result = pangolin_core::db::upsert_domain(db, &d);
            // Phase-1 lifecycle write (issue #45): an auto-issue domain gets a
            // Pending placeholder row in `certs` immediately, so the dashboard
            // and `/certs` table reflect the operator's intent before the
            // background ACME loop has had a chance to tick. The helper is
            // idempotent — pre-existing rows (Issued, Failed, …) are preserved
            // so the operator's history isn't clobbered by a re-save of the
            // domain form.
            if matches!(result, Ok(())) && d.auto_issue {
                let _ = pangolin_core::db::ensure_pending_cert_row(db, &d.domain, &cert_dir);
            }
            // fix/cert_www: refresh the domain → cert link cache. If the
            // upsert succeeded, recompute this domain's link (it may have
            // gained a cert, or the cert it points to may have changed).
            // Failures are logged and ignored — a stale cache entry is
            // recoverable via `load_from_db` at next startup.
            if result.is_ok()
                && let Err(e) = cert_links.relink_for_domain(db, &d.domain)
            {
                log::warn!(
                    "cert_links: relink for {} after create failed: {}",
                    d.domain,
                    e
                );
            }
            result
        })
        .await;

    match result {
        Ok(()) => {
            app.reload_indexes().await;
            Ok(redirect_response("/domains"))
        }
        Err(e) => {
            render_create_page_with_error(app, &format!("Database error: {e}"), csrf, None, lang)
                .await
        }
    }
}

/// POST /api/domains/{domain}/edit — update an existing domain's
/// non-PK fields (site_name, enabled, auto_issue, dns_provider).
///
/// Issue #57 lifts the deliberate post-creation immutability of
/// domains. The PK (`domain` itself) is preserved and the existing
/// `created_at` is kept — only the operator-editable fields can change.
/// Validation is copied from `handle_create` so the wildcard
/// (DNS-01) and DNS-provider-exists invariants survive the edit path.
///
/// On success, `reload_indexes().await` fires `dns_change_notify`,
/// waking the `AcmeState` background loop so the cert state machine
/// (Pending/Issuing/Issued/Failed/Skipped) reacts immediately to
/// changes in `enabled` / `auto_issue` / `dns_provider`.
pub async fn handle_update(
    app: &Arc<App>,
    domain: Option<String>,
    body: &[u8],
    csrf: &str,
    lang: Lang,
) -> http::Result<Resp> {
    let Some(domain_pk) = domain else {
        return Ok(crate::not_found(lang));
    };
    let params = parse_form(body);

    // Look up the existing row — 404 if absent. The PK is immutable;
    // we read everything else from the DB and overlay the form fields.
    let existing = {
        // The read runs on the blocking pool (issue #254).
        let key = domain_pk.clone();
        app.with_db_or(move |db| pangolin_core::db::get_domain(db, &key).unwrap_or(None))
            .await
    };
    let Some(existing) = existing else {
        return Ok(crate::not_found(lang));
    };

    // The whole form is validated by one shared parser (issue #185):
    // the JSON API validates through the very same function.
    //
    // The parse reads the DNS providers, so it runs on the blocking
    // pool with the connection in hand (issue #254).
    let domain_for_db = domain_pk.clone();
    let existing_for_db = existing.clone();
    let parsed = match app
        .with_db(move |conn| {
            parse_domain_form(
                &params,
                &domain_for_db,
                Some(&existing_for_db),
                &|name| {
                    pangolin_core::db::get_dns_provider(conn, name)
                        .unwrap_or(None)
                        .is_some()
                },
                lang,
            )
        })
        .await
    {
        Ok(parsed) => parsed,
        Err(e) => {
            log::warn!("domains: the form could not be parsed: {e}");
            return render_edit_page_with_error(
                app,
                &domain_pk,
                &lang.t("domains.error.form_check_failed"),
                csrf,
                &existing,
                lang,
            )
            .await;
        }
    };
    let parsed = match parsed {
        Ok(p) => p,
        Err(msg) => {
            return render_edit_page_with_error(app, &domain_pk, &msg, csrf, &existing, lang).await;
        }
    };

    let updated = pangolin_core::types::Domain {
        domain: existing.domain.clone(),
        site_name: parsed.site_name,
        enabled: parsed.enabled,
        auto_issue: parsed.auto_issue,
        dns_provider: parsed.dns_provider,
        challenge_kind: parsed.challenge_kind,
        // PK and created_at are immutable; preserve them.
        created_at: existing.created_at,
    };

    // The write runs on the blocking pool, under one connection: the
    // domain row, its cert placeholder and the link-cache refresh
    // belong to the same write (issue #254).
    let cert_links = app.cert_links.clone();
    let cert_dir = app.cert_manager.cert_dir.clone();
    let result = app
        .with_db_res(move |db| {
            let result = pangolin_core::db::upsert_domain(db, &updated);
            // Phase-1 lifecycle write (issue #45): if the operator turned
            // auto-issue on (or left it on) and there's no cert row yet, plant
            // a Pending placeholder so the dashboard reflects intent before
            // the background ACME loop ticks. The helper is idempotent so
            // pre-existing rows (Issued, Failed, …) are preserved.
            if matches!(result, Ok(())) && updated.auto_issue {
                let _ = pangolin_core::db::ensure_pending_cert_row(db, &updated.domain, &cert_dir);
            }
            // fix/cert_www: refresh the cert link. See handle_create for
            // the rationale; same warning-on-failure pattern.
            if result.is_ok()
                && let Err(e) = cert_links.relink_for_domain(db, &updated.domain)
            {
                log::warn!(
                    "cert_links: relink for {} after update failed: {}",
                    updated.domain,
                    e
                );
            }
            result
        })
        .await;

    match result {
        Ok(()) => {
            // Wake AcmeState so cert issuance / state transition
            // follows the field change immediately rather than on the
            // next tick.
            app.reload_indexes().await;
            Ok(redirect_response("/domains"))
        }
        Err(e) => {
            render_edit_page_with_error(
                app,
                &domain_pk,
                &format!("Database error: {e}"),
                csrf,
                &existing,
                lang,
            )
            .await
        }
    }
}

/// Re-render the Edit-domain form with an inline error. Mirrors
/// `render_create_page_with_error` but populates the form with the
/// existing row's values (site_name, dns_provider, auto_issue) so the
/// operator doesn't lose their in-flight edits.
async fn render_edit_page_with_error(
    app: &Arc<App>,
    domain_pk: &str,
    error: &str,
    csrf: &str,
    existing: &pangolin_core::types::Domain,
    lang: Lang,
) -> http::Result<Resp> {
    // The reads run on the blocking pool, under one connection
    // (issue #254).
    let (sites, dns_providers) = app
        .with_db_or(|db| {
            (
                pangolin_core::db::list_sites(db).unwrap_or_default(),
                pangolin_core::db::list_dns_providers(db).unwrap_or_default(),
            )
        })
        .await;
    let html = DomainsEditTemplate {
        sites,
        dns_providers,
        error: Some(error),
        active_nav: "domains",
        preselected_site: Some(existing.site_name.clone()),
        preselected_site_name: Some(existing.site_name.clone()),
        dns_provider_value: existing.dns_provider.clone().unwrap_or_default(),
        auto_issue_checked: existing.auto_issue,
        edit_domain: Some(domain_pk.to_string()),
        current_auto_issue: existing.auto_issue,
        enabled_checked: existing.enabled,
        challenge_kind_value: existing
            .challenge_kind
            .map(|k| k.as_str().to_string())
            .unwrap_or_default(),
        lang,
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

/// Re-render the New-domain form with an inline error. Uses the new
/// `pages/domains/new.html` template via `DomainsNewTemplate` (rather
/// than the legacy single-struct form). Behaviour matches the
/// pre-refactor: re-fetches sites and DNS providers so the dropdowns
/// are populated, and applies the inline error to the form.
async fn render_create_page_with_error(
    app: &Arc<App>,
    error: &str,
    csrf: &str,
    preselected_site: Option<&str>,
    lang: Lang,
) -> http::Result<Resp> {
    // The reads run on the blocking pool, under one connection
    // (issue #254).
    let (sites, dns_providers) = app
        .with_db_or(|db| {
            (
                pangolin_core::db::list_sites(db).unwrap_or_default(),
                pangolin_core::db::list_dns_providers(db).unwrap_or_default(),
            )
        })
        .await;
    let html = DomainsNewTemplate {
        sites,
        dns_providers,
        error: Some(error),
        active_nav: "domains",
        preselected_site: preselected_site.map(String::from),
        preselected_site_name: preselected_site.map(String::from),
        dns_provider_value: String::new(),
        auto_issue_checked: false,
        edit_domain: None,
        current_auto_issue: false,
        enabled_checked: true,
        challenge_kind_value: String::new(),
        lang,
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

/// DEPRECATED: use `DELETE /api/domains/{domain}` (`api_handle_delete`
/// below). This form-POST handler is kept as a fallback during the
/// migration window (issue #48). All admin UIs now use the
/// `templates/components/_hx_delete_button.html` partial instead.
pub async fn handle_delete(
    app: &Arc<App>,
    domain: Option<String>,
    _csrf: &str,
    _lang: Lang,
) -> http::Result<Resp> {
    if let Some(d) = domain
        && !d.is_empty()
    {
        let key = d.clone();
        let _ = app
            .with_db_res(move |db| pangolin_core::db::delete_domain(db, &key))
            .await;
        // fix/cert_www: drop this domain's link from the cache. Any
        // cert row that previously covered it is left in place — the
        // cert may still be serving other domains — but this one
        // should no longer claim it.
        app.cert_links.remove_domain(&d);
        app.reload_indexes().await;
    }
    Ok(redirect_response("/domains"))
}

/// HTMX DELETE /api/domains/{domain} — returns an empty 200 body so HTMX
/// (with hx-swap="delete") can drop the row.
pub async fn api_handle_delete(
    app: &Arc<App>,
    domain: String,
    _csrf: &str,
    lang: Lang,
) -> http::Result<Resp> {
    if domain.is_empty() {
        return Ok(crate::not_found(lang));
    }
    let key = domain.clone();
    let _ = app
        .with_db_res(move |db| pangolin_core::db::delete_domain(db, &key))
        .await;
    // fix/cert_www: see handle_delete above.
    app.cert_links.remove_domain(&domain);
    app.reload_indexes().await;
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::new()))
        .unwrap())
}
