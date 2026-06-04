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
    let session_token = cookie_header
        .and_then(|h| state::parse_session_cookie(h))
        .filter(|t| sessions.is_valid(t));

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
    let path = path.trim_start_matches("/admin").trim_start_matches('/');

    let res: Response<Full<Bytes>> = match path {
        "" | "dashboard" => {
            routes::dashboard::render(&app).await?
        }
        "sites" => {
            routes::sites::render(&app).await?
        }
        "domains" => {
            routes::domains::render(&app).await?
        }
        "tun" => {
            routes::tun::render(&app).await?
        }
        "tokens" => {
            routes::tokens::render(&app).await?
        }
        "certs" => {
            routes::certs::render(&app).await?
        }
        // htmx partials (return HTML fragments)
        "api/sites" if method == "GET" => {
            routes::sites::render_table(&app).await?
        }
        "api/sites/new" => {
            routes::sites::render_form_new(&app).await?
        }
        "api/sites/edit" => {
            routes::sites::render_form_edit(&app, query_param_opt(&body, "name")).await?
        }
        "api/sites" if method == "POST" => {
            routes::sites::handle_create(&app, &body).await?
        }
        "api/sites" if method == "PUT" => {
            routes::sites::handle_update(&app, query_param_opt(&body, "name"), &body).await?
        }
        "api/sites" if method == "DELETE" => {
            routes::sites::handle_delete(&app, query_param_opt(&body, "name")).await?
        }
        "api/domains" if method == "GET" => {
            routes::domains::render_table(&app).await?
        }
        "api/domains" if method == "POST" => {
            routes::domains::handle_create(&app, &body).await?
        }
        "api/domains" if method == "DELETE" => {
            routes::domains::handle_delete(&app, query_param_opt(&body, "domain")).await?
        }
        "api/tokens" if method == "GET" => {
            routes::tokens::render_table(&app).await?
        }
        "api/tokens/new" => {
            routes::tokens::render_form_new(&app).await?
        }
        "api/tokens" if method == "POST" => {
            routes::tokens::handle_create(&app, &body).await?
        }
        "api/tokens" if method == "DELETE" => {
            routes::tokens::handle_delete(&app, query_param_opt(&body, "token")).await?
        }
        "api/certs" if method == "GET" => {
            routes::certs::render_table(&app).await?
        }
        "api/certs" if method == "POST" => {
            routes::certs::handle_create(&app, &body).await?
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

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}