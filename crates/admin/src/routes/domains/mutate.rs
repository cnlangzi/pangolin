//! Domains POST / DELETE handlers.
//!
//! Behaviour is preserved byte-for-byte from the pre-refactor
//! `routes/domains.rs` — only file layout changed.

use askama::Template;
use std::sync::Arc;

use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use crate::templates::{DomainsEditTemplate, DomainsNewTemplate};
use crate::{App, redirect_response};

type Resp = Response<Full<Bytes>>;

fn ok_html(body: String) -> http::Result<Resp> {
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .expect("response builder for 200 OK should not fail"))
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

pub async fn handle_create(app: &Arc<App>, body: &[u8], csrf: &str) -> http::Result<Resp> {
    let params = parse_form(body);
    let domain = params.get("domain").cloned().unwrap_or_default();
    let site_name = params.get("site_name").cloned().unwrap_or_default();
    let auto_issue = params.get("auto_issue").map(|_| true).unwrap_or(false);
    // New form renders the `enabled` checkbox pre-checked (matches the
    // historical `enabled: true` default for new domains). Operators
    // can uncheck it at create time to insert a paused domain.
    let enabled = params.get("enabled").map(|_| true).unwrap_or(true);
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
    // Per-domain challenge kind (issue #55). Form value is one of
    // `"auto"` (NULL — planner picks), `"http-01"`, `"dns-01"`,
    // `"dns-persist-01"`. Anything else (empty, garbage) is treated
    // as `"auto"` to preserve the legacy behaviour and keep the
    // form forgiving when the dropdown is missing.
    let challenge_kind_raw = params
        .get("challenge_kind")
        .cloned()
        .unwrap_or_default()
        .trim()
        .to_string();
    let challenge_kind = match challenge_kind_raw.as_str() {
        "http-01" => Some(pangolin_core::types::ChallengeKind::Http01),
        "dns-01" => Some(pangolin_core::types::ChallengeKind::Dns01),
        "dns-persist-01" => Some(pangolin_core::types::ChallengeKind::DnsPersist01),
        // "auto" or anything we don't recognise — leave the column NULL
        // so the planner applies the auto default (dns-01 with a
        // provider, http-01 without).
        _ => None,
    };

    if domain.is_empty() {
        return render_create_page_with_error(app, "Domain name is required", csrf, None).await;
    }
    if site_name.is_empty() {
        return render_create_page_with_error(app, "Please select a site", csrf, None).await;
    }
    if !pangolin_core::is_valid_domain(&domain) {
        return render_create_page_with_error(
            app,
            "Invalid domain format (use example.com, no scheme)",
            csrf,
            None,
        )
        .await;
    }
    // Wildcard domains must have a DNS association (DNS-01 is the only
    // way to validate `*.example.com`).
    if domain.starts_with("*.") && dns_provider.is_none() {
        return render_create_page_with_error(
            app,
            "Wildcard domains require a DNS provider for DNS-01 validation. \
             Add one under DNS first, then assign it to this domain.",
            csrf,
            None,
        )
        .await;
    }
    // Issue #55 / RFC 8555 §8.3: a wildcard SAN cannot be validated
    // with http-01. Reject the combination at save time so the
    // operator sees a clear error before the ACME server refuses.
    // The error message MUST contain the literal "RFC 8555 §8.3" —
    // the planner emits the same string and the admin UI test grep
    // for it.
    if domain.starts_with("*.")
        && matches!(
            challenge_kind,
            Some(pangolin_core::types::ChallengeKind::Http01)
        )
    {
        return render_create_page_with_error(
            app,
            "Wildcard domains cannot use the http-01 challenge (ACME \
             servers do not offer an http-01 challenge for wildcard \
             identifiers per RFC 8555 §8.3). Set the challenge kind \
             to 'dns-01' or 'dns-persist-01', or pick 'auto' (the \
             planner will resolve to dns-01 when a DNS provider is \
             linked).",
            csrf,
            None,
        )
        .await;
    }
    // If a DNS provider is referenced, verify it exists.
    if let Some(ref name) = dns_provider {
        let db = app.db.lock().await;
        let exists = pangolin_core::db::get_dns_provider(&db, name)
            .unwrap_or(None)
            .is_some();
        drop(db);
        if !exists {
            return render_create_page_with_error(
                app,
                &format!("DNS provider '{name}' does not exist; create it under DNS first"),
                csrf,
                None,
            )
            .await;
        }
    }

    let d = pangolin_core::types::Domain {
        domain,
        site_name,
        enabled,
        auto_issue,
        dns_provider,
        challenge_kind,
        created_at: chrono::Utc::now(),
    };

    let db = app.db.lock().await;
    let result = pangolin_core::db::upsert_domain(&db, &d);
    // Phase-1 lifecycle write (issue #45): an auto-issue domain gets a
    // Pending placeholder row in `certs` immediately, so the dashboard
    // and `/certs` table reflect the operator's intent before the
    // background ACME loop has had a chance to tick. The helper is
    // idempotent — pre-existing rows (Issued, Failed, …) are preserved
    // so the operator's history isn't clobbered by a re-save of the
    // domain form.
    if matches!(result, Ok(())) && d.auto_issue {
        let _ =
            pangolin_core::db::ensure_pending_cert_row(&db, &d.domain, &app.cert_manager.cert_dir);
    }
    drop(db);

    match result {
        Ok(()) => {
            app.reload_indexes().await;
            Ok(redirect_response("/domains"))
        }
        Err(e) => {
            render_create_page_with_error(app, &format!("Database error: {}", e), csrf, None).await
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
) -> http::Result<Resp> {
    let Some(domain_pk) = domain else {
        return Ok(crate::not_found());
    };
    let params = parse_form(body);

    // Look up the existing row — 404 if absent. The PK is immutable;
    // we read everything else from the DB and overlay the form fields.
    let existing = {
        let db = app.db.lock().await;
        pangolin_core::db::get_domain(&db, &domain_pk).unwrap_or(None)
    };
    let Some(existing) = existing else {
        return Ok(crate::not_found());
    };

    let site_name = params
        .get("site_name")
        .cloned()
        .unwrap_or_else(|| existing.site_name.clone());
    if site_name.is_empty() {
        return render_edit_page_with_error(
            app,
            &domain_pk,
            "Please select a site",
            csrf,
            &existing,
        )
        .await;
    }
    // Form fields:
    //   site_name     — required, drops through to existing if missing
    //   enabled       — checkbox: present => true, absent => false
    //   auto_issue    — checkbox: present => true, absent => false
    //   dns_provider  — text, empty => None
    // The form always sends the current value of every field (the
    // page is a fully-rendered form), so `params.get("enabled")`
    // being absent only happens on a malformed client; we default to
    // the existing value to be safe.
    let enabled = params
        .get("enabled")
        .map(|_| true)
        .unwrap_or(existing.enabled);
    let auto_issue = params
        .get("auto_issue")
        .map(|_| true)
        .unwrap_or(existing.auto_issue);
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

    // Per-domain challenge kind (issue #55). The edit form sends the
    // current value of the dropdown. Same parsing rules as
    // `handle_create`: `"http-01"` / `"dns-01"` / `"dns-persist-01"`
    // map to the matching enum variant; `"auto"` or anything else
    // (empty, missing field) maps to `None` so the planner applies
    // the auto default. The field is dropped through to the existing
    // value when the form is missing it (malformed client fallback).
    let challenge_kind_raw = params
        .get("challenge_kind")
        .cloned()
        .unwrap_or_default()
        .trim()
        .to_string();
    let challenge_kind = match challenge_kind_raw.as_str() {
        "http-01" => Some(pangolin_core::types::ChallengeKind::Http01),
        "dns-01" => Some(pangolin_core::types::ChallengeKind::Dns01),
        "dns-persist-01" => Some(pangolin_core::types::ChallengeKind::DnsPersist01),
        _ => existing.challenge_kind,
    };

    // Wildcard domains must have a DNS association (DNS-01 is the only
    // way to validate `*.example.com`). Invariant copied from
    // `handle_create`.
    if domain_pk.starts_with("*.") && dns_provider.is_none() {
        return render_edit_page_with_error(
            app,
            &domain_pk,
            "Wildcard domains require a DNS provider for DNS-01 validation. \
             Add one under DNS first, then assign it to this domain.",
            csrf,
            &existing,
        )
        .await;
    }
    // Issue #55 / RFC 8555 §8.3: a wildcard SAN cannot be validated
    // with http-01. Mirror the save-time check from `handle_create`
    // so the edit path refuses the same combination.
    if domain_pk.starts_with("*.")
        && matches!(
            challenge_kind,
            Some(pangolin_core::types::ChallengeKind::Http01)
        )
    {
        return render_edit_page_with_error(
            app,
            &domain_pk,
            "Wildcard domains cannot use the http-01 challenge (ACME \
             servers do not offer an http-01 challenge for wildcard \
             identifiers per RFC 8555 §8.3). Set the challenge kind \
             to 'dns-01' or 'dns-persist-01', or pick 'auto' (the \
             planner will resolve to dns-01 when a DNS provider is \
             linked).",
            csrf,
            &existing,
        )
        .await;
    }
    // If a DNS provider is referenced, verify it exists. Invariant
    // copied from `handle_create`.
    if let Some(ref name) = dns_provider {
        let db = app.db.lock().await;
        let exists = pangolin_core::db::get_dns_provider(&db, name)
            .unwrap_or(None)
            .is_some();
        drop(db);
        if !exists {
            return render_edit_page_with_error(
                app,
                &domain_pk,
                &format!("DNS provider '{name}' does not exist; create it under DNS first"),
                csrf,
                &existing,
            )
            .await;
        }
    }

    let updated = pangolin_core::types::Domain {
        domain: existing.domain.clone(),
        site_name,
        enabled,
        auto_issue,
        dns_provider,
        challenge_kind,
        // PK and created_at are immutable; preserve them.
        created_at: existing.created_at,
    };

    let db = app.db.lock().await;
    let result = pangolin_core::db::upsert_domain(&db, &updated);
    // Phase-1 lifecycle write (issue #45): if the operator turned
    // auto-issue on (or left it on) and there's no cert row yet, plant
    // a Pending placeholder so the dashboard reflects intent before
    // the background ACME loop ticks. The helper is idempotent so
    // pre-existing rows (Issued, Failed, …) are preserved.
    if matches!(result, Ok(())) && updated.auto_issue {
        let _ = pangolin_core::db::ensure_pending_cert_row(
            &db,
            &updated.domain,
            &app.cert_manager.cert_dir,
        );
    }
    drop(db);

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
                &format!("Database error: {}", e),
                csrf,
                &existing,
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
) -> http::Result<Resp> {
    let db = app.db.lock().await;
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    let dns_providers = pangolin_core::db::list_dns_providers(&db).unwrap_or_default();
    drop(db);
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
) -> http::Result<Resp> {
    let db = app.db.lock().await;
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    let dns_providers = pangolin_core::db::list_dns_providers(&db).unwrap_or_default();
    drop(db);
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
) -> http::Result<Resp> {
    if let Some(d) = domain
        && !d.is_empty()
    {
        let db = app.db.lock().await;
        let _ = pangolin_core::db::delete_domain(&db, &d);
        drop(db);
        app.reload_indexes().await;
    }
    Ok(redirect_response("/domains"))
}

/// HTMX DELETE /api/domains/{domain} — returns an empty 200 body so HTMX
/// (with hx-swap="delete") can drop the row.
pub async fn api_handle_delete(app: &Arc<App>, domain: String, _csrf: &str) -> http::Result<Resp> {
    if domain.is_empty() {
        return Ok(crate::not_found());
    }
    let db = app.db.lock().await;
    let _ = pangolin_core::db::delete_domain(&db, &domain);
    drop(db);
    app.reload_indexes().await;
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::new()))
        .unwrap())
}
