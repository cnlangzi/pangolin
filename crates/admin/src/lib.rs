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

pub mod routes;
pub mod state;
pub mod templates;
pub mod app;

use std::sync::Arc;

use bytes::Bytes;
use http::{Response, StatusCode, header};
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
    body: Bytes,
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
        let mut resp = Response::new(Full::new(Bytes::from(format!("Redirecting to {}", location))));
        *resp.status_mut() = StatusCode::FOUND;
        resp.headers_mut().insert(
            header::LOCATION,
            header::HeaderValue::from_str(&location)
                .map_err(http::Error::from)?,
        );
        return Ok(resp);
    }

    // ── Route dispatch ───────────────────────────────────────────────
    // ── CSRF check on mutating methods ──────────────────────────────
    // Skip CSRF for login (no session yet) and logout (CSRF is part of auth).
    let is_login = path == "/admin/login" || path == "/admin/login/";
    let is_logout = path == "/admin/logout";
    if matches!(method, "POST" | "PUT" | "PATCH" | "DELETE")
        && !is_login
        && !is_logout
    {
        let session_token_str = session_token.as_deref().unwrap_or("");
        let csrf_from_form = query_param_opt(&body, "_csrf");
        let csrf_from_cookie = cookie_header.and_then(state::parse_csrf_cookie);
        let csrf = csrf_from_form.or(csrf_from_cookie);
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
        "" | "dashboard" => {
            routes::dashboard::render(&app, &csrf_token).await?
        }
        "sites" => {
            routes::sites::render(&app, &csrf_token).await?
        }
        "domains" => {
            routes::domains::render(&app, &csrf_token).await?
        }
        "tun" => {
            routes::tun::render(&app, &csrf_token).await?
        }
        "tokens" => {
            routes::tokens::render(&app, &csrf_token).await?
        }
        "certs" => {
            routes::certs::render(&app, &csrf_token).await?
        }
        // htmx partials (return HTML fragments)
        "api/sites" if method == "GET" => {
            routes::sites::render_table(&app, &csrf_token).await?
        }
        "api/sites/new" => {
            routes::sites::render_form_new(&app, &csrf_token).await?
        }
        "api/sites/edit" => {
            routes::sites::render_form_edit(&app, query_param_opt(&body, "name"), &csrf_token).await?
        }
        "api/sites" if method == "POST" => {
            routes::sites::handle_create(&app, &body, &csrf_token).await?
        }
        "api/sites" if method == "PUT" => {
            routes::sites::handle_update(&app, query_param_opt(&body, "name"), &body, &csrf_token).await?
        }
        "api/sites" if method == "DELETE" => {
            routes::sites::handle_delete(&app, query_param_opt(&body, "name"), &csrf_token).await?
        }
        "api/domains" if method == "GET" => {
            routes::domains::render_table(&app, &csrf_token).await?
        }
        "api/domains" if method == "POST" => {
            routes::domains::handle_create(&app, &body, &csrf_token).await?
        }
        "api/domains" if method == "DELETE" => {
            routes::domains::handle_delete(&app, query_param_opt(&body, "domain"), &csrf_token).await?
        }
        "api/tokens" if method == "GET" => {
            routes::tokens::render_table(&app, &csrf_token).await?
        }
        "api/tokens/new" => {
            routes::tokens::render_form_new(&app, &csrf_token).await?
        }
        "api/tokens" if method == "POST" => {
            routes::tokens::handle_create(&app, &body, &csrf_token).await?
        }
        "api/tokens" if method == "DELETE" => {
            routes::tokens::handle_delete(&app, query_param_opt(&body, "token"), &csrf_token).await?
        }
        "api/certs" if method == "GET" => {
            routes::certs::render_table(&app, &csrf_token).await?
        }
        "api/certs/new" => {
            routes::certs::render_form_new(&csrf_token).await?
        }
        "api/certs" if method == "POST" => {
            routes::certs::handle_create(&app, &body, &csrf_token).await?
        }
        // Auth
        "login" => {
            if method == "POST" {
                routes::auth::handle_login(&app, sessions, &body).await?
            } else {
                routes::auth::render_login(query_param_opt(&body, "next").as_deref()).await?
            }
        }
        "logout" if method == "POST" => {
            routes::auth::handle_logout(sessions, &session_token.unwrap(), &body).await?
        }
        _ => not_found(),
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
    let safe: String = csrf
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
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
    body_str
        .split('&')
        .find_map(|pair| {
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