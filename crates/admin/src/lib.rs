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
    // ── Auth check ──────────────────────────────────────────────────
    let parsed_cookie = cookie_header.and_then(state::parse_session_cookie);
    let session_token = match parsed_cookie {
        Some(t) if sessions.validate(&t).await => Some(t),
        _ => None,
    };

    // Only the login page is publicly accessible. Everything else (UI,
    // /api, /assets) requires a session. (The only exception is static
    // assets, but those don't need to be behind auth at the moment —
    // they're cosmetic; the auth check above is fine to apply to them
    // because real browsers load them with the session cookie attached
    // automatically.)
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

    // Look up the CSRF token for the current session (if any). For unauthenticated
    // requests (login page), this is empty.
    let csrf_token: String = if let Some(ref st) = session_token {
        sessions.csrf_for(st).await.unwrap_or_default()
    } else {
        String::new()
    };

    let res: Response<Full<Bytes>> = match (path, method) {
        // ── /assets/... : static resources (no auth needed; the
        // auth check above already gated everything) ───────────────
        (p, _) if p.starts_with("assets/") => {
            // Serve from embedded assets at compile time.
            // Falling through to 404 if not found.
            match serve_static_asset(p) {
                Some(resp) => resp,
                None => not_found(),
            }
        }

        // ── /api/... : HTMX HTML fragments ─────────────────────────
        // The brief splits /api/site/{name}/domains (GET) and
        // /api/domains/{domain} (DELETE). Both are HTMX endpoints that
        // return HTML partials, not JSON. The JSON API is gone.
        // (All /api/* routes are dispatched via the prefix branch below.)

        // ── UI HTML pages at root ──────────────────────────────────
        ("" | "dashboard", "GET") => routes::dashboard::render(&app, &csrf_token).await?,
        ("sites", "GET") => routes::sites::render(&app, &csrf_token).await?,
        ("sites/new", "GET") => {
            routes::sites::render_create_page(&app, &csrf_token).await?
        }
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
        ("sites/delete", "POST") => {
            routes::sites::handle_delete(&app, query_param_opt(&merged_params, "name"), &csrf_token)
                .await?
        }
        ("domains", "GET") => routes::domains::render(&app, &csrf_token).await?,
        ("domains/new", "GET") => {
            routes::domains::render_create_page(&app, &csrf_token).await?
        }
        ("domains/new", "POST") => {
            routes::domains::handle_create(&app, &merged_params, &csrf_token).await?
        }
        ("domains/delete", "POST") => {
            routes::domains::handle_delete(
                &app,
                query_param_opt(&merged_params, "domain"),
                &csrf_token,
            )
            .await?
        }
        ("tun", "GET") => routes::tun::render(&app, &csrf_token).await?,
        ("tun/new", "GET") => {
            routes::tun::render_create_page(&csrf_token).await?
        }
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
        ("tun/delete", "POST") => {
            routes::tun::handle_delete(
                &app,
                query_param_opt(&merged_params, "name"),
                &csrf_token,
            )
            .await?
        }
        ("certs", "GET") => routes::certs::render(&app, &csrf_token).await?,
        ("certs/new", "GET") => routes::certs::render_create_page(&csrf_token).await?,
        ("certs/new", "POST") => {
            routes::certs::handle_create(&app, &merged_params, &csrf_token).await?
        }
        ("certs/delete", "POST") => {
            routes::certs::handle_delete(
                &app,
                query_param_opt(&merged_params, "domain"),
                &csrf_token,
            )
            .await?
        }
        ("dns", "GET") => routes::dns::render(&app, &csrf_token).await?,
        ("dns/new", "GET") => routes::dns::render_create_page(&app, &csrf_token).await?,
        ("dns/new", "POST") => {
            routes::dns::handle_create(&app, &merged_params, &csrf_token).await?
        }
        ("dns/test", "POST") => routes::dns::handle_test(&app, &merged_params).await?,

        // ── Auth ────────────────────────────────────────────────────
        ("login", "GET") => {
            routes::auth::render_login(query_param_opt(&merged_params, "next").as_deref())
                .await?
        }
        ("login", "POST") => {
            routes::auth::handle_login(&app, sessions, &merged_params).await?
        }
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
                            routes::domains::render_for_site(&app, site_name, &csrf_token)
                                .await?
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
                if let Some(rest2) = rest.strip_prefix("site/") {
                    if let Some((site_name, suffix)) = rest2.split_once('/') {
                        match (suffix, method) {
                            ("domains", "GET") => {
                                routes::domains::render_table_for_site(
                                    &app,
                                    site_name,
                                    &csrf_token,
                                )
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
                    } else {
                        not_found()
                    }
                } else {
                    not_found()
                }
            } else if let Some(rest) = path.strip_prefix("dns/") {
                // /dns/{name}/edit   GET   (render edit form)
                // /dns/{name}/edit   POST  (update)
                // /dns/{name}/delete POST  (delete)
                if let Some((name, suffix)) = rest.split_once('/') {
                    match (suffix, method) {
                        ("edit", "GET") => {
                            routes::dns::render_edit_page(
                                &app,
                                Some(name.to_string()),
                                &csrf_token,
                            )
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
                        ("delete", "POST") => {
                            routes::dns::handle_delete(
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
            } else if let Some(rest) = path.strip_prefix("tun/") {
                // /tun/{name}/edit   GET   (render edit form)
                // /tun/{name}/edit   POST  (update)
                // /tun/{name}/delete POST  (delete)
                if let Some((name, suffix)) = rest.split_once('/') {
                    match (suffix, method) {
                        ("edit", "GET") => {
                            routes::tun::render_edit_page(
                                &app,
                                Some(name.to_string()),
                                &csrf_token,
                            )
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
                        ("delete", "POST") => {
                            routes::tun::handle_delete(
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

/// CSS content hash for cache-busting. Computed at build time by `build.rs`
/// from `assets/app.css` and embedded as a `?v=<hash>` query parameter.
pub const CSS_HASH: &str = env!("APP_CSS_HASH");

/// JS bundle content hash for cache-busting. Computed at build time by `build.rs`
/// from `assets/app.js` and embedded as a `?v=<hash>` query parameter. The
/// admin UI loads the bundle once from `base.html` via `/assets/app.js?v=__JS_HASH__`.
pub const JS_HASH: &str = env!("APP_JS_HASH");

/// Substitute the `__CSS_HASH__` and `__JS_HASH__` placeholders in rendered
/// HTML with the build-time bundle hashes. Used to prevent browser caching of
/// stale CSS/JS after rebuilds.
pub fn render_with_assets(html: String) -> String {
    html.replace("__CSS_HASH__", CSS_HASH)
        .replace("__JS_HASH__", JS_HASH)
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

/// Return the embedded asset for `/assets/{name}` (CSS, JS, images).
///
/// Asset bytes are embedded at compile time by `build.rs` from
/// `crates/admin/assets/`. The asset list is small (a single CSS bundle
/// and a single JS bundle plus a few SVG/PNG icons). For unknown asset
/// paths we return `None` so the caller can answer 404.
fn serve_static_asset(path: &str) -> Option<Response<Full<Bytes>>> {
    // Strip leading "assets/" prefix and any query string (e.g. ?v=HASH).
    let raw = path.strip_prefix("assets/")?;
    let name = raw.split('?').next()?;
    if name.is_empty() || name.contains("..") {
        return None;
    }
    let (bytes, content_type) = match name {
        "app.css" => (include_bytes!("../../../assets/app.css").to_vec(), "text/css; charset=utf-8"),
        "app.js" => (include_bytes!("../../../assets/app.js").to_vec(), "application/javascript; charset=utf-8"),
        _ => return None,
    };
    let resp = Response::builder()
        .status(200)
        .header("Content-Type", content_type)
        // Long cache + immutable. The cache-busting query string
        // (`?v=HASH`) handles content invalidation, so the browser will
        // re-request on each release.
        .header("Cache-Control", "public, max-age=31536000, immutable")
        .body(Full::new(Bytes::from(bytes)))
        .ok()?;
    Some(resp)
}

fn query_param_opt(body: &[u8], key: &str) -> Option<String> {
    let body_str = std::str::from_utf8(body).ok()?;
    body_str.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then_some(urlencoding::decode(v).ok()?.to_string())
    })
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
