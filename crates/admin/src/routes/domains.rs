//! Domains route — list / new / delete (full-page, no modal).

use askama::Template;
use std::sync::Arc;

use bytes::Bytes;
use http::{Response, StatusCode};
use http_body_util::Full;

use crate::templates::DomainFormTemplate;
use crate::{redirect_response, App};

fn ok_html(body: String) -> http::Result<Response<Full<Bytes>>> {
    let resp = Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .expect("response builder for 200 OK should not fail");
    Ok(resp)
}

pub async fn render(app: &Arc<App>, csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    let db = app.db.lock().await;
    let domains = pangolin_core::db::list_domains(&db).unwrap_or_default();
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    drop(db);
    ok_html(crate::render_with_assets_and_csrf(
        crate::templates::DomainsTemplate {
            domains,
            sites,
            active_nav: "domains",
        }
        .render()
        .unwrap(),
        csrf,
    ))
}

pub async fn render_create_page(app: &Arc<App>, csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    let db = app.db.lock().await;
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    let dns_providers = pangolin_core::db::list_dns_providers(&db).unwrap_or_default();
    drop(db);
    let html = DomainFormTemplate {
        sites,
        dns_providers,
        error: None,
        active_nav: "domains",
        preselected_site: None,
        preselected_site_name: None,
        dns_provider_value: String::new(),
        auto_issue_checked: false,
        edit_domain: None,
        current_auto_issue: false,
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

/// Render domains filtered by a specific site — used for /admin/site/{name}/domains.
pub async fn render_for_site(
    app: &Arc<App>,
    site_name: &str,
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    let db = app.db.lock().await;
    let all_domains = pangolin_core::db::list_domains(&db).unwrap_or_default();
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    let site = sites.iter().find(|s| s.name == site_name).cloned();
    drop(db);

    let site_name_owned = site_name.to_owned();
    let domains: Vec<_> = all_domains
        .into_iter()
        .filter(|d| d.site_name == site_name_owned)
        .collect();

    let site = match site {
        Some(s) => s,
        None => {
            let mut resp = Response::new(Full::new(Bytes::from(
                "<p class='text-red-500 text-sm'>Site not found</p>",
            )));
            *resp.status_mut() = StatusCode::NOT_FOUND;
            return Ok(resp);
        }
    };

    ok_html(crate::render_with_assets_and_csrf(
        crate::templates::SiteDomainsTemplate {
            site,
            domains,
            sites,
            active_nav: "sites",
        }
        .render()
        .unwrap(),
        csrf,
    ))
}

/// Render only the table rows for site-specific domains (htmx partial).
pub async fn render_table_for_site(
    app: &Arc<App>,
    site_name: &str,
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    let db = app.db.lock().await;
    let all_domains = pangolin_core::db::list_domains(&db).unwrap_or_default();
    drop(db);
    let site_name_owned = site_name.to_owned();
    let domains: Vec<_> = all_domains
        .into_iter()
        .filter(|d| d.site_name == site_name_owned)
        .collect();

    let rows: Vec<String> = domains
        .iter()
        .map(|d| {
            format!(
                r##"<tr id="domain-{}" class="border-b border-slate-100 dark:border-slate-700 hover:bg-slate-50 dark:hover:bg-slate-700/30 transition-colors">
  <td class="py-3 px-3"><span class="font-mono text-sm text-slate-800 dark:text-slate-100">{}</span></td>
  <td class="py-3 px-3">
    <span class="inline-flex items-center gap-1 text-xs font-medium px-2 py-0.5 rounded-full {} {} {}">{}</span>
  </td>
  <td class="py-3 px-3">
    <div class="flex items-center gap-1">
      <form method="POST" action="/admin/domains/delete" onsubmit="return confirm('Delete domain {}?');" class="inline">
        <input type="hidden" name="domain" value="{}">
        <input type="hidden" name="_csrf" value="__CSRF__">
        <button type="submit"
          class="text-slate-400 dark:text-slate-400 hover:text-red-500 dark:hover:text-red-400 p-1.5 rounded hover:bg-red-50 dark:hover:bg-red-500/10 transition-colors"
          title="Delete">
          <svg class="w-4 h-4" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" d="M14.74 9l-.346 9m-4.788 0L9.26 9m9.968-3.21c.342.052.682.107 1.022.166m-1.022-.165L18.16 19.673a2.25 2.25 0 01-2.244 2.077H8.084a2.25 2.25 0 01-2.244-2.077L4.772 5.79m14.456 0a48.108 48.108 0 00-3.478-.397m-12 .562c.34-.059.68-.114 1.022-.165m0 0a48.11 48.11 0 013.478-.397m7.5 0v-.916c0-1.18-.91-2.164-2.09-2.201a51.964 51.964 0 00-3.32 0c-1.18.037-2.09 1.022-2.09 2.201v.916m7.5 0a48.667 48.667 0 00-7.5 0"/></svg>
        </button>
      </form>
    </div>
  </td>
</tr>"##,
                d.domain,
                d.domain,
                if d.enabled {
                    "bg-green-100 text-green-700"
                } else {
                    "bg-slate-100 text-slate-400"
                },
                if d.enabled {
                    "dark:bg-green-900/30 dark:text-green-300"
                } else {
                    "dark:bg-slate-700 dark:text-slate-500"
                },
                if d.enabled { "" } else { "line-through" },
                if d.enabled { "enabled" } else { "disabled" },
                d.domain,
                d.domain
            )
        })
        .collect();

    ok_html(crate::render_with_assets_and_csrf(rows.join(""), csrf))
}

/// Render the "new domain" form pre-seeded with a site name (from site-specific sub-page).
pub async fn api_render_form_new(
    app: &Arc<App>,
    site_name: &str,
    _body: &[u8],
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    let db = app.db.lock().await;
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    let dns_providers = pangolin_core::db::list_dns_providers(&db).unwrap_or_default();
    drop(db);
    let html = DomainFormTemplate {
        sites,
        dns_providers,
        error: None,
        active_nav: "domains",
        preselected_site: Some(site_name.to_string()),
        preselected_site_name: Some(site_name.to_string()),
        dns_provider_value: String::new(),
        auto_issue_checked: false,
        edit_domain: None,
        current_auto_issue: false,
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

pub async fn handle_create(
    app: &Arc<App>,
    body: &[u8],
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    let params = parse_form(body);
    let domain = params.get("domain").cloned().unwrap_or_default();
    let site_name = params.get("site_name").cloned().unwrap_or_default();
    let auto_issue = params.get("auto_issue").map(|_| true).unwrap_or(false);
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
    let next = sanitize_next_redirect(params.get("next").cloned().unwrap_or_default());

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
        enabled: true,
        auto_issue,
        dns_provider,
        created_at: chrono::Utc::now(),
    };

    let db = app.db.lock().await;
    let result = pangolin_core::db::upsert_domain(&db, &d);
    drop(db);

    match result {
        Ok(()) => {
            app.reload_indexes().await;
            Ok(redirect_response(&next))
        }
        Err(e) => {
            render_create_page_with_error(app, &format!("Database error: {}", e), csrf, None).await
        }
    }
}

async fn render_create_page_with_error(
    app: &Arc<App>,
    error: &str,
    csrf: &str,
    preselected_site: Option<&str>,
) -> http::Result<Response<Full<Bytes>>> {
    let db = app.db.lock().await;
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    let dns_providers = pangolin_core::db::list_dns_providers(&db).unwrap_or_default();
    drop(db);
    let html = DomainFormTemplate {
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
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

pub async fn handle_delete(
    app: &Arc<App>,
    domain: Option<String>,
    _csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    if let Some(d) = domain {
        if !d.is_empty() {
            let db = app.db.lock().await;
            let _ = pangolin_core::db::delete_domain(&db, &d);
            drop(db);
            app.reload_indexes().await;
        }
    }
    Ok(redirect_response("/admin/domains"))
}

/// Render the "edit domain" form pre-filled with the current values for an
/// existing domain. Used by the per-row Edit button on the site_domains
/// page (htmx swap into the modal).
pub async fn api_render_form_edit(
    app: &Arc<App>,
    domain: &str,
    _body: &[u8],
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    let db = app.db.lock().await;
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    let dns_providers = pangolin_core::db::list_dns_providers(&db).unwrap_or_default();
    let existing = pangolin_core::db::list_domains(&db)
        .unwrap_or_default()
        .into_iter()
        .find(|d| d.domain == domain);
    drop(db);

    let Some(existing) = existing else {
        let mut resp = Response::new(Full::new(Bytes::from(
            "<p class='text-red-500 text-sm p-4'>Domain not found.</p>",
        )));
        *resp.status_mut() = StatusCode::NOT_FOUND;
        return Ok(resp);
    };

    let html = DomainFormTemplate {
        sites,
        dns_providers,
        error: None,
        active_nav: "domains",
        preselected_site: Some(existing.site_name.clone()),
        preselected_site_name: Some(existing.site_name.clone()),
        dns_provider_value: existing.dns_provider.clone().unwrap_or_default(),
        auto_issue_checked: existing.auto_issue,
        edit_domain: Some(existing.domain.clone()),
        current_auto_issue: existing.auto_issue,
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

/// Handle POST /admin/api/domains/{domain}/edit — update an existing
/// domain's auto_issue / dns_provider / enabled fields. Domain name and
/// site_name are immutable (the form locks them); this endpoint validates
/// the DNS provider reference and wildcard constraint, then upserts.
pub async fn handle_update(
    app: &Arc<App>,
    domain: &str,
    body: &[u8],
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    let params = parse_form(body);
    let auto_issue = params.get("auto_issue").map(|_| true).unwrap_or(false);
    let dns_provider = params
        .get("dns_provider")
        .cloned()
        .unwrap_or_default()
        .trim()
        .to_string();
    let next = sanitize_next_redirect(params.get("next").cloned().unwrap_or_default());
    let dns_provider = if dns_provider.is_empty() {
        None
    } else {
        Some(dns_provider)
    };

    // Load the existing row to preserve immutable fields (domain, site_name,
    // created_at) and to read the current `enabled` flag.
    let existing = {
        let db = app.db.lock().await;
        pangolin_core::db::list_domains(&db)
            .unwrap_or_default()
            .into_iter()
            .find(|d| d.domain == domain)
    };
    let Some(existing) = existing else {
        let mut resp = Response::new(Full::new(Bytes::from(
            "<p class='text-red-500 text-sm p-4'>Domain not found.</p>",
        )));
        *resp.status_mut() = StatusCode::NOT_FOUND;
        return Ok(resp);
    };

    // Wildcard domains must keep a DNS association.
    if existing.domain.starts_with("*.") && dns_provider.is_none() {
        return render_edit_page_with_error(
            app,
            domain,
            "Wildcard domains require a DNS provider for DNS-01 validation.",
            csrf,
        )
        .await;
    }
    if let Some(ref name) = dns_provider {
        let db = app.db.lock().await;
        let exists = pangolin_core::db::get_dns_provider(&db, name)
            .unwrap_or(None)
            .is_some();
        drop(db);
        if !exists {
            return render_edit_page_with_error(
                app,
                domain,
                &format!("DNS provider '{name}' does not exist; create it under DNS first"),
                csrf,
            )
            .await;
        }
    }

    let updated = pangolin_core::types::Domain {
        domain: existing.domain.clone(),
        site_name: existing.site_name.clone(),
        enabled: existing.enabled,
        auto_issue,
        dns_provider,
        created_at: existing.created_at,
    };

    let db = app.db.lock().await;
    let result = pangolin_core::db::upsert_domain(&db, &updated);
    drop(db);

    match result {
        Ok(()) => {
            app.reload_indexes().await;
            // `next` defaults to /admin/domains; the edit form is only
            // reachable from a site_domains page, so the form's hidden
            // `next` field carries the correct /admin/site/.../domains
            // URL back to us.
            Ok(redirect_response(&next))
        }
        Err(e) => {
            render_edit_page_with_error(app, domain, &format!("Database error: {}", e), csrf).await
        }
    }
}

async fn render_edit_page_with_error(
    app: &Arc<App>,
    domain: &str,
    error: &str,
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    let db = app.db.lock().await;
    let sites = pangolin_core::db::list_sites(&db).unwrap_or_default();
    let dns_providers = pangolin_core::db::list_dns_providers(&db).unwrap_or_default();
    let existing = pangolin_core::db::list_domains(&db)
        .unwrap_or_default()
        .into_iter()
        .find(|d| d.domain == domain);
    drop(db);

    let (preselected_site, preselected_site_name, dns_provider_value, auto_issue_checked) =
        match existing {
            Some(d) => (
                Some(d.site_name.clone()),
                Some(d.site_name.clone()),
                d.dns_provider.clone().unwrap_or_default(),
                d.auto_issue,
            ),
            None => (None, None, String::new(), false),
        };

    let html = DomainFormTemplate {
        sites,
        dns_providers,
        error: Some(error),
        active_nav: "domains",
        preselected_site,
        preselected_site_name,
        dns_provider_value,
        auto_issue_checked,
        edit_domain: Some(domain.to_string()),
        current_auto_issue: auto_issue_checked,
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

/// POST /admin/api/domains/{domain}/toggle — flip the `enabled` flag on a
/// single domain row, then return the freshly-rendered row HTML so htmx
/// can swap it in place. The CSRF token is in the form body for browser
/// hx-post requests; we verify it via the standard CSRF check in `handle`.
pub async fn handle_toggle(
    app: &Arc<App>,
    domain: &str,
    csrf: &str,
) -> http::Result<Response<Full<Bytes>>> {
    let (new_state, site_name) = {
        let db = app.db.lock().await;
        let current = pangolin_core::db::list_domains(&db)
            .unwrap_or_default()
            .into_iter()
            .find(|d| d.domain == domain);
        let Some(current) = current else {
            let mut resp = Response::new(Full::new(Bytes::from(
                "<tr><td colspan='3' class='text-red-500 text-sm p-3'>Domain not found.</td></tr>",
            )));
            *resp.status_mut() = StatusCode::NOT_FOUND;
            return Ok(resp);
        };
        let new_state = !current.enabled;
        let updated = pangolin_core::db::set_domain_enabled(&db, domain, new_state).unwrap_or(false);
        if !updated {
            let mut resp = Response::new(Full::new(Bytes::from(
                "<tr><td colspan='3' class='text-red-500 text-sm p-3'>Toggle failed.</td></tr>",
            )));
            *resp.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            return Ok(resp);
        }
        (new_state, current.site_name.clone())
    };
    // Reload indexes so the routing layer picks up the new enabled state.
    app.reload_indexes().await;

    // Re-render the row so htmx can swap it in place.
    let row = render_domain_row(domain, new_state, &site_name);
    ok_html(crate::render_with_assets_and_csrf(row, csrf))
}

/// Render a single domain row for htmx swap. Used by handle_toggle to
/// return the new HTML fragment for the row.
fn render_domain_row(domain: &str, enabled: bool, site_name: &str) -> String {
    format!(
        r##"<tr id="domain-{domain}" class="hover:bg-slate-50 dark:hover:bg-slate-700/30 transition-colors">
  <td class="px-4 py-3">
    <span class="font-mono text-sm text-slate-800 dark:text-slate-100">{domain}</span>
    <div class="text-xs text-slate-500 dark:text-slate-400 mt-0.5">site: {site_name}</div>
  </td>
  <td class="px-4 py-3">
    <button hx-post="/admin/api/domains/{domain}/toggle"
      hx-vals='{{"_csrf": "__CSRF__"}}'
      hx-swap="outerHTML"
      hx-target="#domain-{domain}"
      class="inline-flex items-center gap-1 text-xs font-medium px-2 py-0.5 rounded-full transition-colors {badge_bg} {badge_dark} {strike}"
      title="Click to toggle">
      {badge_label}
    </button>
  </td>
  <td class="px-4 py-3">
    <div class="flex items-center justify-end gap-1">
      <button hx-get="/admin/api/domains/{domain}/edit"
        hx-target="#modal-body"
        class="text-slate-400 dark:text-slate-400 hover:text-accent-500 dark:hover:text-accent-400 p-1.5 rounded hover:bg-accent-50 dark:hover:bg-accent-500/10 transition-colors"
        title="Edit"
        aria-label="Edit {domain}">
        <svg class="w-4 h-4" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" d="M16.862 4.487l1.687-1.688a1.875 1.875 0 112.652 2.652L10.582 16.07a4.5 4.5 0 01-1.897 1.13L6 18l.8-2.685a4.5 4.5 0 011.13-1.897l8.932-8.931z"/></svg>
      </button>
      <form method="POST" action="/admin/domains/delete" onsubmit="return confirm('Delete domain {domain}?');" class="inline">
        <input type="hidden" name="domain" value="{domain}">
        <input type="hidden" name="_csrf" value="__CSRF__">
        <button type="submit"
          class="text-slate-400 dark:text-slate-400 hover:text-red-500 dark:hover:text-red-400 p-1.5 rounded hover:bg-red-50 dark:hover:bg-red-500/10 transition-colors"
          title="Delete"
          aria-label="Delete {domain}">
          <svg class="w-4 h-4" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" d="M14.74 9l-.346 9m-4.788 0L9.26 9m9.968-3.21c.342.052.682.107 1.022.166m-1.022-.165L18.16 19.673a2.25 2.25 0 01-2.244 2.077H8.084a2.25 2.25 0 01-2.244-2.077L4.772 5.79m14.456 0a48.108 48.108 0 00-3.478-.397m-12 .562c.34-.059.68-.114 1.022-.165m0 0a48.11 48.11 0 013.478-.397m7.5 0v-.916c0-1.18-.91-2.164-2.09-2.201a51.964 51.964 0 00-3.32 0c-1.18.037-2.09 1.022-2.09 2.201v.916m7.5 0a48.667 48.667 0 00-7.5 0"/></svg>
        </button>
      </form>
    </div>
  </td>
</tr>"##,
        domain = domain,
        site_name = site_name,
        badge_bg = if enabled { "bg-green-100 text-green-700" } else { "bg-slate-100 text-slate-400" },
        badge_dark = if enabled { "dark:bg-green-900/30 dark:text-green-300" } else { "dark:bg-slate-700 dark:text-slate-500" },
        strike = if enabled { "" } else { "line-through" },
        badge_label = if enabled { "enabled" } else { "disabled" },
    )
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

/// Validates a user-supplied `next` redirect path. Only paths starting with
/// `/admin/` are accepted — this prevents open-redirect attacks where a
/// crafted form submission could redirect the user to an external site.
/// Falls back to `/admin/domains` for empty / invalid input.
fn sanitize_next_redirect(next: String) -> String {
    if next.starts_with("/admin/") && !next.contains("://") && !next.contains('\n') {
        return next;
    }
    "/admin/domains".to_string()
}
