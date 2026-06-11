//! Auth: login / logout handlers.
//!
//! Sessions are in-memory in `SessionStore`. Cookie is `HttpOnly; SameSite=Strict`.

use askama::Template;
use bytes::Bytes as Buf;
use http::{Response, StatusCode};
use http_body_util::Full;

use crate::state::{
    make_csrf_cookie, make_logout_cookie, make_logout_csrf_cookie, make_session_cookie,
    SessionStore,
};
use crate::templates::LoginTemplate;
use crate::App;

/// Serve the login page HTML.
pub async fn render_login(next: Option<&str>) -> http::Result<Response<Full<Buf>>> {
    let tmpl = LoginTemplate {
        next: next.unwrap_or(""),
        error: "",
    };
    let html = tmpl
        .render()
        .unwrap_or_else(|e| format!("Template error: {}", e));
    crate::ok_html_with_csrf(html, "")
}

/// Handle POST /admin/login form submission.
pub async fn handle_login(
    app: &App,
    sessions: &SessionStore,
    body: &[u8],
) -> http::Result<Response<Full<Buf>>> {
    let body_str = std::str::from_utf8(body).unwrap_or("");
    let mut form_username = String::new();
    let mut form_password = String::new();
    let mut form_next = String::new();

    for pair in body_str.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            match k {
                "username" => {
                    form_username = urlencoding::decode(v).unwrap_or_default().to_string()
                }
                "password" => {
                    form_password = urlencoding::decode(v).unwrap_or_default().to_string()
                }
                "next" => form_next = urlencoding::decode(v).unwrap_or_default().to_string(),
                _ => {}
            }
        }
    }

    if form_username == app.config.admin.username && form_password == app.config.admin.password {
        let (token, csrf) = sessions.create_session(&form_username).await;
        let redirect_to = if form_next.is_empty() {
            "/".to_string()
        } else {
            form_next
        };
        let resp = Response::builder()
            .status(StatusCode::FOUND)
            .header("Location", &redirect_to)
            .header("Set-Cookie", make_session_cookie(&token))
            .header("Set-Cookie", make_csrf_cookie(&csrf))
            .body(Full::new(Buf::from("Redirecting...")))
            .unwrap();
        return Ok(resp);
    }

    let error_html =
        r#"<p class="text-red-400 text-sm mb-4 font-medium">Invalid username or password</p>"#;
    let tmpl = LoginTemplate {
        next: "",
        error: error_html,
    };
    let html = tmpl
        .render()
        .unwrap_or_else(|e| format!("Template error: {}", e));
    crate::ok_html_with_csrf(html, "")
}

/// Handle POST /admin/logout.
pub async fn handle_logout(
    sessions: &SessionStore,
    token: &str,
    _body: &[u8],
) -> http::Result<Response<Full<Buf>>> {
    sessions.destroy(token).await;
    let resp = Response::builder()
        .status(StatusCode::FOUND)
        .header("Location", "/login")
        .header("Set-Cookie", make_logout_cookie())
        .header("Set-Cookie", make_logout_csrf_cookie())
        .body(Full::new(Buf::from("Redirecting...")))
        .unwrap();
    Ok(resp)
}
