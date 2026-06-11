//! Pangolin admin UI — SSR templates (askama) + htmx.
//! No JS framework. TailwindCSS compiled by npm run build.
//!
//! ## Integration with ngx
//!
//! ngx `serve.rs` routes `/admin/*` requests here. This crate exposes a
//! single `handle()` function that returns an `http::Response`.
//!
//! ## Auth
//!
//! Sessions are in-memory `HashMap<token, Instant>`. A random 32-byte hex
//! token is stored in a `HttpOnly; SameSite=Strict` cookie.
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

    let is_auth_page = path == "/admin/login" || path == "/admin/login/";

    if session_token.is_none() && !is_auth_page {
        let next = path.trim_start_matches("/admin");
        let location = if next.is_empty() || next == "/" {
            "/admin/login".to_string()
        } else {
            format!("/admin/login?next={}", urlencoding::encode(next))
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

    // ── Route dispatch ───────────────────────────────────────────────
    // ── CSRF check on mutating methods ──────────────────────────────
    // Skip CSRF for login (no session yet) and logout (CSRF is part of auth).
    let is_login = path == "/admin/login" || path == "/admin/login/";
    let is_logout = path == "/admin/logout";
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

    let path = path.trim_start_matches("/admin").trim_start_matches('/');

    // Look up the CSRF token for the current session (if any). For unauthenticated
    // requests (login page), this is empty.
    let csrf_token: String = if let Some(ref st) = session_token {
        sessions.csrf_for(st).await.unwrap_or_default()
    } else {
        String::new()
    };

    let res: Response<Full<Bytes>> = match path {
        "" | "dashboard" => routes::dashboard::render(&app, &csrf_token).await?,
        "sites" if method == "GET" => routes::sites::render(&app, &csrf_token).await?,
        "sites/new" if method == "GET" => {
            routes::sites::render_create_page(&app, &csrf_token).await?
        }
        "sites/new" if method == "POST" => {
            routes::sites::handle_create(&app, &merged_params, &csrf_token).await?
        }
        "sites/edit" if method == "GET" => {
            routes::sites::render_edit_page(
                &app,
                query_param_opt(&merged_params, "name"),
                &csrf_token,
            )
            .await?
        }
        "sites/edit" if method == "POST" => {
            routes::sites::handle_update(
                &app,
                query_param_opt(&merged_params, "name"),
                &merged_params,
                &csrf_token,
            )
            .await?
        }
        "sites/delete" if method == "POST" => {
            routes::sites::handle_delete(&app, query_param_opt(&merged_params, "name"), &csrf_token)
                .await?
        }
        "domains" if method == "GET" => routes::domains::render(&app, &csrf_token).await?,
        "domains/new" if method == "GET" => {
            routes::domains::render_create_page(&app, &csrf_token).await?
        }
        "domains/new" if method == "POST" => {
            routes::domains::handle_create(&app, &merged_params, &csrf_token).await?
        }
        "domains/delete" if method == "POST" => {
            routes::domains::handle_delete(
                &app,
                query_param_opt(&merged_params, "domain"),
                &csrf_token,
            )
            .await?
        }
        "api/domains" if method == "POST" => {
            // Generic create endpoint (used by the site_domains form when
            // the site is locked — site_name is taken from the form body,
            // which already has the correct preselected value).
            routes::domains::handle_create(&app, &merged_params, &csrf_token).await?
        }
        "tun" => routes::tun::render(&app, &csrf_token).await?,
        "tokens" if method == "GET" => routes::tokens::render(&app, &csrf_token).await?,
        "tokens/new" if method == "GET" => {
            routes::tokens::render_create_page(&app, &csrf_token).await?
        }
        "tokens/new" if method == "POST" => {
            routes::tokens::handle_create(&app, &merged_params, &csrf_token).await?
        }
        "tokens/delete" if method == "POST" => {
            routes::tokens::handle_delete(
                &app,
                query_param_opt(&merged_params, "token"),
                &csrf_token,
            )
            .await?
        }
        "certs" if method == "GET" => routes::certs::render(&app, &csrf_token).await?,
        "certs/new" if method == "GET" => routes::certs::render_create_page(&csrf_token).await?,
        "certs/new" if method == "POST" => {
            routes::certs::handle_create(&app, &merged_params, &csrf_token).await?
        }
        "certs/delete" if method == "POST" => {
            routes::certs::handle_delete(
                &app,
                query_param_opt(&merged_params, "domain"),
                &csrf_token,
            )
            .await?
        }
        "dns" if method == "GET" => routes::dns::render(&app, &csrf_token).await?,
        "dns/new" if method == "GET" => routes::dns::render_create_page(&app, &csrf_token).await?,
        "dns/new" if method == "POST" => {
            routes::dns::handle_create(&app, &merged_params, &csrf_token).await?
        }
        "dns/edit" if method == "GET" => {
            routes::dns::render_edit_page(
                &app,
                query_param_opt(&merged_params, "name"),
                &csrf_token,
            )
            .await?
        }
        "dns/edit" if method == "POST" => {
            routes::dns::handle_update(
                &app,
                query_param_opt(&merged_params, "name"),
                &merged_params,
                &csrf_token,
            )
            .await?
        }
        "dns/delete" if method == "POST" => {
            routes::dns::handle_delete(&app, query_param_opt(&merged_params, "name"), &csrf_token)
                .await?
        }
        // Auth
        "login" => {
            if method == "POST" {
                routes::auth::handle_login(&app, sessions, &merged_params).await?
            } else {
                routes::auth::render_login(query_param_opt(&merged_params, "next").as_deref())
                    .await?
            }
        }
        "logout" if method == "POST" => {
            routes::auth::handle_logout(sessions, &session_token.unwrap(), &merged_params).await?
        }
        // ── Site-specific sub-pages ─────────────────────────────────────
        _ => {
            // Prefix-based dispatch for: site/{name}/domains, site/{name}/api/domains, etc.
            if let Some(rest) = path.strip_prefix("site/") {
                if let Some((site_name, suffix)) = rest.split_once('/') {
                    match suffix {
                        "domains" => {
                            routes::domains::render_for_site(&app, site_name, &csrf_token).await?
                        }
                        "api/domains" if method == "GET" => {
                            routes::domains::render_table_for_site(&app, site_name, &csrf_token)
                                .await?
                        }
                        "api/domains" if method == "POST" => {
                            routes::domains::handle_create(&app, &merged_params, &csrf_token)
                                .await?
                        }
                        "api/domains/new" => {
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
            } else if let Some(rest) = path.strip_prefix("api/domains/") {
                // Global domain-scoped endpoints (no site context).
                // Path: api/domains/{domain}/{action}
                if let Some((domain, action)) = rest.split_once('/') {
                    match (action, method) {
                        ("edit", "GET") => {
                            routes::domains::api_render_form_edit(
                                &app,
                                domain,
                                &merged_params,
                                &csrf_token,
                            )
                            .await?
                        }
                        ("edit", "POST") => {
                            routes::domains::handle_update(
                                &app,
                                domain,
                                &merged_params,
                                &csrf_token,
                            )
                            .await?
                        }
                        ("toggle", "POST") => {
                            // `view` query param selects the response shape:
                            //   "row"  → desktop table <tr>  (default, no param)
                            //   "card" → mobile card <div>   (set by site_domains.html)
                            // The mobile card must replace the whole card
                            // div (which contains Edit / Delete buttons and
                            // the DNS line), not just the toggle badge.
                            let view = query_param_opt(&merged_params, "view")
                                .unwrap_or_else(|| "row".to_string());
                            routes::domains::handle_toggle(&app, domain, &view, &csrf_token)
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

/// Substitute the `__CSS_HASH__` placeholder in rendered HTML with the build-time
/// CSS hash. Used to prevent browser caching of stale CSS after rebuilds.
pub fn render_with_assets(html: String) -> String {
    html.replace("__CSS_HASH__", CSS_HASH)
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
            <a href="/admin/" class="text-sm text-red-700 underline mt-2 inline-block">← Back to dashboard</a>
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
