//! Pangolin admin UI — SSR templates (askama) + htmx.
//! No JS framework. TailwindCSS compiled by npm run build.
//!
//! ## Integration with ngx
//!
//! ngx `serve.rs` routes requests to `admin::handle()` for the three
//! dashboard namespaces:
//!   - `/...`          UI HTML pages (root + /sites, /domains, /tun, ...)
//!   - `/api/...`      HTMX HTML fragments (partials for in-place updates)
//!   - `/assets/...`   static resources (CSS, JS, images)
//!
//! ## Auth
//!
//! Sessions are in-memory `HashMap<token, Instant>`. A random 32-byte hex
//! token is stored in a `HttpOnly; SameSite=Strict` cookie with `Path=/`
//! (so it's sent on every dashboard URL, not just the legacy `/admin/`
//! prefix that no longer exists).
//!
//! ## Routing
//!
//! All routes are registered in `routes/mod.rs`. This lib re-exports the
//! `App` type from `pangolin-core` for convenience.

pub mod app;
pub mod assets;
pub mod routes;
pub mod state;
pub mod templates;

use std::sync::Arc;

use bytes::Bytes;
use http::{header, Response, StatusCode};
use http_body_util::Full;

pub use app::App;

/// Unified entry point for all admin HTTP requests.
/// Called from ngx `serve.rs`.
pub async fn handle(
    app: Arc<App>,
    sessions: &state::SessionStore,
    path: &str,
    method: &str,
    cookie_header: Option<&str>,
    _body: Bytes,
    merged_params: Bytes,
) -> http::Result<Response<Full<Bytes>>> {
    // ── Static assets: serve immediately without auth ──────────────
    // Static resources (CSS, JS, images) are served directly from the
    // embedded public/ directory without requiring authentication.
    // This prevents 302 redirects to /login for asset requests.
    let trimmed_path = path.trim_start_matches('/');
    if trimmed_path.starts_with("assets/") {
        return Ok(match serve_static_asset(trimmed_path) {
            Some(resp) => resp,
            None => not_found(),
        });
    }

    // ── Auth check ──────────────────────────────────────────────────
    let parsed_cookie = cookie_header.and_then(state::parse_session_cookie);
    let session_token = match parsed_cookie {
        Some(t) if sessions.validate(&t).await => Some(t),
        _ => None,
    };

    // Only the login page is publicly accessible. Everything else (UI, /api)
    // requires a session.
    let is_auth_page = path == "/login" || path == "/login/";

    if session_token.is_none() && !is_auth_page {
        let next = if path == "/" || path.is_empty() {
            "/".to_string()
        } else {
            path.to_string()
        };
        let location = if next == "/" || next.is_empty() {
            "/login".to_string()
        } else {
            format!("/login?next={}", urlencoding::encode(&next))
        };
        let mut resp = Response::new(Full::new(Bytes::from(format!(
            "Redirecting to {}",
            location
        ))));
        *resp.status_mut() = StatusCode::FOUND;
        resp.headers_mut().insert(
            header::LOCATION,
            header::HeaderValue::from_str(&location).map_err(http::Error::from)?,
        );
        return Ok(resp);
    }

    // ── CSRF check on mutating methods ──────────────────────────────
    // Skip CSRF for login (no session yet) and logout (CSRF is part of auth).
    let is_login = path == "/login" || path == "/login/";
    let is_logout = path == "/logout";
    if matches!(method, "POST" | "PUT" | "PATCH" | "DELETE") && !is_login && !is_logout {
        let session_token_str = session_token.as_deref().unwrap_or("");
        // CSRF token MUST come from the body only, not from URL query string
        // CSRF token is read from merged_params (body + URL query string
        // combined). For POST/PUT/PATCH it comes from the hidden _csrf field
        // in the form body. For DELETE it comes from the URL query string
        // (since DELETE body is skipped in serve.rs to avoid idle-hang).
        // The browser cannot forge query string parameters cross-origin
        // (SameSite=Strict cookies block cross-origin requests anyway).
        let csrf = query_param_opt(&merged_params, "_csrf");
        match csrf {
            Some(t) if sessions.verify_csrf(session_token_str, &t).await => {}
            _ => {
                return Ok(forbidden_response(
                    "CSRF token missing or invalid. Please reload the page and try again.",
                ));
            }
        }
    }

    // ── Route dispatch ───────────────────────────────────────────────
    let path = path.trim_start_matches('/');
    // Percent-decode the path so URL-encoded path parameters (e.g.
    // `%2A.wildcard.com` for `*.wildcard.com`) reach the DB layer in
    // their canonical form. pingora/ngx hands us the raw URI path
    // without decoding it.
    let path = percent_decode_path(path);

    // Look up the CSRF token for the current session (if any). For unauthenticated
    // requests (login page), this is empty.
    let csrf_token: String = if let Some(ref st) = session_token {
        sessions.csrf_for(st).await.unwrap_or_default()
    } else {
        String::new()
    };

    let res: Response<Full<Bytes>> = match (path.as_str(), method) {
        // ── /api/... : HTMX HTML fragments ─────────────────────────
        // The brief splits /api/site/{name}/domains (GET) and
        // /api/domains/{domain} (DELETE). Both are HTMX endpoints that
        // return HTML partials, not JSON. The JSON API is gone.
        // (All /api/* routes are dispatched via the prefix branch below.)

        // ── UI HTML pages at root ──────────────────────────────────
        ("" | "dashboard", "GET") => routes::dashboard::render(&app, &csrf_token).await?,
        ("sites", "GET") => routes::sites::render(&app, &csrf_token).await?,
        ("sites/new", "GET") => routes::sites::render_create_page(&app, &csrf_token).await?,
        ("sites/new", "POST") => {
            routes::sites::handle_create(&app, &merged_params, &csrf_token).await?
        }
        ("sites/edit", "GET") => {
            routes::sites::render_edit_page(
                &app,
                query_param_opt(&merged_params, "name"),
                &csrf_token,
            )
            .await?
        }
        ("sites/edit", "POST") => {
            routes::sites::handle_update(
                &app,
                query_param_opt(&merged_params, "name"),
                &merged_params,
                &csrf_token,
            )
            .await?
        }
        // DEPRECATED: use DELETE /api/sites/{name}. Kept as a fallback during the
        // migration window (issue #48).
        ("sites/delete", "POST") => {
            routes::sites::handle_delete(&app, query_param_opt(&merged_params, "name"), &csrf_token)
                .await?
        }
        ("domains", "GET") => routes::domains::render(&app, &csrf_token).await?,
        ("domains/new", "GET") => routes::domains::render_create_page(&app, &csrf_token).await?,
        ("domains/new", "POST") => {
            routes::domains::handle_create(&app, &merged_params, &csrf_token).await?
        }
        // DEPRECATED: use DELETE /api/domains/{domain}. Kept as a fallback during the
        // migration window (issue #48).
        ("domains/delete", "POST") => {
            routes::domains::handle_delete(
                &app,
                query_param_opt(&merged_params, "domain"),
                &csrf_token,
            )
            .await?
        }
        ("tun", "GET") => routes::tun::render(&app, &csrf_token).await?,
        ("tun/new", "GET") => routes::tun::render_create_page(&csrf_token).await?,
        ("tun/new", "POST") => {
            routes::tun::handle_create(&app, &merged_params, &csrf_token).await?
        }
        ("tun/edit", "GET") => {
            routes::tun::render_edit_page(
                &app,
                query_param_opt(&merged_params, "name"),
                &csrf_token,
            )
            .await?
        }
        ("tun/edit", "POST") => {
            routes::tun::handle_update(
                &app,
                query_param_opt(&merged_params, "name"),
                &merged_params,
                &csrf_token,
            )
            .await?
        }
        // DEPRECATED: use DELETE /api/tun/{name}. Kept as a fallback during the
        // migration window (issue #48).
        ("tun/delete", "POST") => {
            routes::tun::handle_delete(&app, query_param_opt(&merged_params, "name"), &csrf_token)
                .await?
        }
        ("certs", "GET") => {
            routes::certs::render(
                &app,
                query_param_opt(&merged_params, "status").as_deref(),
                &csrf_token,
            )
            .await?
        }
        ("certs/new", "GET") => routes::certs::render_create_page(&csrf_token).await?,
        ("certs/new", "POST") => {
            routes::certs::handle_create(&app, &merged_params, &csrf_token).await?
        }
        ("certs/retry", "POST") => {
            routes::certs::handle_retry(&app, &merged_params, &csrf_token).await?
        }
        // DEPRECATED: use DELETE /api/certs/{domain}. Kept as a fallback during the
        // migration window (issue #48).
        ("certs/delete", "POST") => {
            routes::certs::handle_delete(
                &app,
                query_param_opt(&merged_params, "domain"),
                &csrf_token,
            )
            .await?
        }
        ("api/certs/summary", "GET") => routes::certs::handle_summary(&app).await?,
        ("dns", "GET") => routes::dns::render(&app, &csrf_token).await?,
        ("dns/new", "GET") => routes::dns::render_create_page(&app, &csrf_token).await?,
        ("dns/new", "POST") => {
            routes::dns::handle_create(&app, &merged_params, &csrf_token).await?
        }
        ("dns/test", "POST") => routes::dns::handle_test(&app, &merged_params).await?,

        // ── System operations ───────────────────────────────────────
        ("api/reload", "POST") => routes::system::handle_reload(&app).await?,

        // ── Auth ────────────────────────────────────────────────────
        ("login", "GET") => {
            routes::auth::render_login(query_param_opt(&merged_params, "next").as_deref()).await?
        }
        ("login", "POST") => routes::auth::handle_login(&app, sessions, &merged_params).await?,
        ("logout", "POST") => {
            routes::auth::handle_logout(sessions, &session_token.unwrap(), &merged_params).await?
        }

        // ── Site-specific sub-pages (UI) and HTMX fragments (/api/) ─
        _ => {
            // UI: /site/{name}/domains
            if let Some(rest) = path.strip_prefix("site/") {
                if let Some((site_name, suffix)) = rest.split_once('/') {
                    match suffix {
                        "domains" => {
                            routes::domains::render_for_site(&app, site_name, &csrf_token).await?
                        }
                        _ => not_found(),
                    }
                } else {
                    not_found()
                }
            } else if let Some(rest) = path.strip_prefix("api/") {
                // /api/site/{name}/domains       GET   (HTML partial: site-specific domain table rows)
                // /api/site/{name}/domains/new   GET   (HTML partial: new-domain form, preselected)
                // /api/domains/{domain}          DELETE (HTML fragment — empty body on success)
                // /api/sites/{name}              DELETE (HTML fragment — empty body on success)
                // /api/certs/{domain}            DELETE (HTML fragment — empty body on success)
                // /api/tun/{name}                DELETE (HTML fragment — empty body on success)
                // /api/dns/{name}                DELETE (HTML fragment — empty body on success)
                if let Some(rest2) = rest.strip_prefix("site/") {
                    if let Some((site_name, suffix)) = rest2.split_once('/') {
                        match (suffix, method) {
                            ("domains", "GET") => {
                                routes::domains::render_table_for_site(&app, site_name, &csrf_token)
                                    .await?
                            }
                            ("domains/new", "GET") => {
                                routes::domains::api_render_form_new(
                                    &app,
                                    site_name,
                                    &merged_params,
                                    &csrf_token,
                                )
                                .await?
                            }
                            _ => not_found(),
                        }
                    } else {
                        not_found()
                    }
                } else if let Some(domain) = rest.strip_prefix("domains/") {
                    if method == "DELETE" {
                        if domain.is_empty() {
                            not_found()
                        } else {
                            routes::domains::api_handle_delete(
                                &app,
                                domain.to_string(),
                                &csrf_token,
                            )
                            .await?
                        }
                    } else if method == "POST" {
                        // POST /api/domains/{domain}/edit — issue #57.
                        // The edit form's `form_action()` resolves to
                        // this URL (see `DomainsEditTemplate::form_action`).
                        // We also accept POST /domains/{domain}/edit (see
                        // the `domains/` prefix branch below) for parity
                        // with the dns/tun resource pattern.
                        if let Some((name, suffix)) = domain.split_once('/') {
                            if name.is_empty() || suffix != "edit" {
                                not_found()
                            } else {
                                routes::domains::handle_update(
                                    &app,
                                    Some(name.to_string()),
                                    &merged_params,
                                    &csrf_token,
                                )
                                .await?
                            }
                        } else {
                            not_found()
                        }
                    } else {
                        not_found()
                    }
                } else if let Some(name) = rest.strip_prefix("sites/") {
                    if method == "DELETE" {
                        if name.is_empty() {
                            not_found()
                        } else {
                            routes::sites::api_handle_delete(&app, name.to_string(), &csrf_token)
                                .await?
                        }
                    } else {
                        not_found()
                    }
                } else if let Some(domain) = rest.strip_prefix("certs/") {
                    if method == "DELETE" {
                        if domain.is_empty() {
                            not_found()
                        } else {
                            routes::certs::api_handle_delete(&app, domain.to_string(), &csrf_token)
                                .await?
                        }
                    } else {
                        not_found()
                    }
                } else if let Some(name) = rest.strip_prefix("tun/") {
                    if method == "DELETE" {
                        if name.is_empty() {
                            not_found()
                        } else {
                            routes::tun::api_handle_delete(&app, name.to_string(), &csrf_token)
                                .await?
                        }
                    } else {
                        not_found()
                    }
                } else if let Some(name) = rest.strip_prefix("dns/") {
                    if method == "DELETE" {
                        if name.is_empty() {
                            not_found()
                        } else {
                            routes::dns::api_handle_delete(&app, name.to_string(), &csrf_token)
                                .await?
                        }
                    } else {
                        not_found()
                    }
                } else {
                    not_found()
                }
            } else if let Some(rest) = path.strip_prefix("domains/") {
                // /domains/{domain}/edit   GET   (render edit form — issue #57)
                // /domains/{domain}/edit   POST  (update — issue #57; see also
                //                              the /api/domains/{domain}/edit
                //                              alias used by the form's
                //                              `form_action()` in the template)
                // /domains/{domain}/delete POST  (delete) — DEPRECATED: use DELETE /api/domains/{domain}.
                if let Some((name, suffix)) = rest.split_once('/') {
                    match (suffix, method) {
                        ("edit", "GET") => {
                            routes::domains::render_edit_page(
                                &app,
                                Some(name.to_string()),
                                &csrf_token,
                            )
                            .await?
                        }
                        ("edit", "POST") => {
                            routes::domains::handle_update(
                                &app,
                                Some(name.to_string()),
                                &merged_params,
                                &csrf_token,
                            )
                            .await?
                        }
                        // DEPRECATED: use DELETE /api/domains/{domain}. Kept as a fallback during
                        // the migration window (issue #48).
                        ("delete", "POST") => {
                            routes::domains::handle_delete(
                                &app,
                                Some(name.to_string()),
                                &csrf_token,
                            )
                            .await?
                        }
                        _ => not_found(),
                    }
                } else {
                    not_found()
                }
            } else if let Some(rest) = path.strip_prefix("dns/") {
                // /dns/{name}/edit   GET   (render edit form)
                // /dns/{name}/edit   POST  (update)
                // /dns/{name}/delete POST  (delete) — DEPRECATED: use DELETE /api/dns/{name}.
                if let Some((name, suffix)) = rest.split_once('/') {
                    match (suffix, method) {
                        ("edit", "GET") => {
                            routes::dns::render_edit_page(&app, Some(name.to_string()), &csrf_token)
                                .await?
                        }
                        ("edit", "POST") => {
                            routes::dns::handle_update(
                                &app,
                                Some(name.to_string()),
                                &merged_params,
                                &csrf_token,
                            )
                            .await?
                        }
                        // DEPRECATED: use DELETE /api/dns/{name}. Kept as a fallback during
                        // the migration window (issue #48).
                        ("delete", "POST") => {
                            routes::dns::handle_delete(&app, Some(name.to_string()), &csrf_token)
                                .await?
                        }
                        _ => not_found(),
                    }
                } else {
                    not_found()
                }
            } else if let Some(rest) = path.strip_prefix("tun/") {
                // /tun/{name}/edit   GET   (render edit form)
                // /tun/{name}/edit   POST  (update)
                // /tun/{name}/delete POST  (delete) — DEPRECATED: use DELETE /api/tun/{name}.
                if let Some((name, suffix)) = rest.split_once('/') {
                    match (suffix, method) {
                        ("edit", "GET") => {
                            routes::tun::render_edit_page(&app, Some(name.to_string()), &csrf_token)
                                .await?
                        }
                        ("edit", "POST") => {
                            routes::tun::handle_update(
                                &app,
                                Some(name.to_string()),
                                &merged_params,
                                &csrf_token,
                            )
                            .await?
                        }
                        // DEPRECATED: use DELETE /api/tun/{name}. Kept as a fallback during
                        // the migration window (issue #48).
                        ("delete", "POST") => {
                            routes::tun::handle_delete(&app, Some(name.to_string()), &csrf_token)
                                .await?
                        }
                        _ => not_found(),
                    }
                } else {
                    not_found()
                }
            } else {
                not_found()
            }
        }
    };

    Ok(res)
}

/// Extract a form field value from a `application/x-www-form-urlencoded` body.
#[allow(dead_code)]
fn query_param(body: &[u8], key: &str) -> String {
    query_param_opt(body, key).unwrap_or_default()
}

/// Substitute the `__CSS_HASH__`, `__JS_FILE__`, and `__JS_HASH__` placeholders
/// in rendered HTML with the runtime asset hashes and active JS filename.
/// Used to prevent browser caching of stale CSS/JS after rebuilds.
pub fn render_with_assets(html: String) -> String {
    html.replace("__CSS_HASH__", &assets::CSS_HASH)
        .replace("__JS_FILE__", *assets::JS_FILE)
        .replace("__JS_HASH__", &assets::JS_HASH)
}

/// Substitute the `__CSRF__` placeholder in rendered HTML with the user's
/// session CSRF token. Templates use this to embed a hidden field in every
/// POST/PUT/DELETE form, enabling CSRF protection on mutating actions.
pub fn render_with_csrf(html: String, csrf: &str) -> String {
    // Escape the CSRF token for safe inclusion in HTML attribute values.
    // Hex-encoded tokens only contain [0-9a-f] so escaping is a no-op, but
    // we still apply minimal sanitisation for defence in depth.
    let safe: String = csrf.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    html.replace("__CSRF__", &safe)
}

/// Apply both asset URL and CSRF placeholder substitution.
pub fn render_with_assets_and_csrf(html: String, csrf: &str) -> String {
    render_with_csrf(render_with_assets(html), csrf)
}

/// Build a 200 OK HTML response with the standard asset URL and CSRF substitutions.
pub fn ok_html_with_csrf(body: String, csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    let bytes = render_with_assets_and_csrf(body, csrf);
    let resp = Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(bytes)))
        .unwrap();
    Ok(resp)
}

/// Return the embedded asset for `/assets/{name}` (CSS, JS, images, vendor files).
///
/// Asset bytes are served from the [`assets::Asset`] rust-embed snapshot of
/// the workspace `assets/` directory (compile-time embed in release; fs-read
/// in debug via the `debug-embed` feature). Unknown paths return `None` so
/// the caller can answer 404.
fn serve_static_asset(path: &str) -> Option<Response<Full<Bytes>>> {
    // Strip leading "assets/" prefix and any query string (e.g. ?v=HASH).
    let raw = path.strip_prefix("assets/")?;
    let name = raw.split('?').next()?;
    if name.is_empty() || name.contains("..") {
        return None;
    }
    let file = <assets::Asset as rust_embed::RustEmbed>::get(name)?;
    let content_type = match name.rsplit('.').next() {
        Some("css") => assets::CSS_MIME,
        Some("js") => assets::JS_MIME,
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    };
    Response::builder()
        .status(200)
        .header("Content-Type", content_type)
        .header("Cache-Control", assets::IMMUTABLE_CACHE)
        .body(Full::new(Bytes::from(file.data.into_owned())))
        .ok()
}

fn query_param_opt(body: &[u8], key: &str) -> Option<String> {
    let body_str = std::str::from_utf8(body).ok()?;
    body_str.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then_some(urlencoding::decode(v).ok()?.to_string())
    })
}

/// Percent-decode each `/`-separated segment of `path` so route params
/// resolve to their canonical form (e.g. `%2A.wildcard.com` →
/// `*.wildcard.com`). ngx/pingora passes the raw URI path without
/// decoding, so URL-encoded path params otherwise mismatch the DB.
/// Decoding is per-segment to avoid treating `/` as decodable.
fn percent_decode_path(path: &str) -> String {
    // Fast path: the common case (login page, list pages, static
    // assets) has no percent escapes, so skip the alloc + per-segment
    // decode work entirely.
    if !path.contains('%') {
        return path.to_string();
    }
    let mut out = String::with_capacity(path.len());
    for (i, segment) in path.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        // urlencoding::decode never expands the result; safe to push
        // raw bytes (the crate emits valid UTF-8 for valid escapes).
        match urlencoding::decode(segment) {
            Ok(decoded) => out.push_str(&decoded),
            // Fall back to the raw segment on decode failure so a
            // malformed path returns 404 (not 500).
            Err(_) => out.push_str(segment),
        }
    }
    out
}

fn not_found() -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(
            r#"<div class="p-6 text-slate-500">Page not found</div>"#,
        )))
        .unwrap()
}

/// Return a 302 Found redirect to the given path.
pub fn redirect_response(location: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, location)
        .body(Full::new(Bytes::new()))
        .unwrap()
}

/// Return a 403 Forbidden response with a brief explanation.
fn forbidden_response(message: &str) -> Response<Full<Bytes>> {
    let body = format!(
        r#"<div class="p-6 max-w-md"><div class="bg-red-50 border border-red-200 rounded-lg p-4">
            <h2 class="text-red-800 font-semibold mb-1">403 Forbidden</h2>
            <p class="text-red-700 text-sm">{}</p>
            <a href="/" class="text-sm text-red-700 underline mt-2 inline-block">← Back to dashboard</a>
        </div></div>"#,
        message
    );
    Response::builder()
        .status(StatusCode::FORBIDDEN)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
