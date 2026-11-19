//! Auth: login / logout handlers.
//!
//! Sessions are in-memory in `SessionStore`. Cookie is `HttpOnly; SameSite=Strict`.

use askama::Template;
use bytes::Bytes as Buf;
use http::{Response, StatusCode};
use http_body_util::Full;

use crate::App;
use crate::i18n::Lang;
use crate::state::{
    SessionStore, make_csrf_cookie, make_logout_cookie, make_logout_csrf_cookie,
    make_session_cookie,
};
use crate::templates::LoginTemplate;

/// Serve the login page HTML.
pub async fn render_login(next: Option<&str>, lang: Lang) -> http::Result<Response<Full<Buf>>> {
    let tmpl = LoginTemplate {
        next: next.unwrap_or(""),
        error: "",
        lang,
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
    lang: Lang,
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

    let error = lang.t("login.error.invalid_credentials");
    let tmpl = LoginTemplate {
        next: "",
        error: &error,
        lang,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Lang;

    /// The body of a rendered response.
    async fn body_of(response: http::Response<Full<Buf>>) -> String {
        let bytes = http_body_util::BodyExt::collect(response.into_body())
            .await
            .expect("body")
            .to_bytes();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// The login page carries the language of the request in its
    /// `<html lang>` attribute.
    #[tokio::test]
    async fn the_login_page_declares_its_language() {
        let html = body_of(render_login(None, Lang::En).await.expect("render")).await;
        assert!(html.contains(r#"<html lang="en">"#), "{html}");

        let html = body_of(render_login(None, Lang::ZhCn).await.expect("render")).await;
        assert!(html.contains(r#"<html lang="zh-CN">"#), "{html}");
    }

    /// The language switcher is rendered exactly when there is more
    /// than one language to choose from.
    ///
    /// The expectation is computed from the catalog, not hard-coded:
    /// the day the second language lands, this test follows.
    #[tokio::test]
    async fn the_language_switcher_follows_the_available_languages() {
        let html = body_of(render_login(None, Lang::En).await.expect("render")).await;
        let expected = Lang::available().len() >= 2;
        assert_eq!(
            html.contains("/lang/"),
            expected,
            "the switcher must be rendered only when there is a choice: {html}"
        );
    }
}
