//! Auth: login / logout handlers.
//!
//! Sessions are in-memory in `SessionStore`. Cookie is `HttpOnly; SameSite=Strict`.

use http::{Response, StatusCode};
use http_body_util::Full;
use bytes::Bytes as Buf;

use crate::state::{
    make_csrf_cookie, make_logout_cookie, make_logout_csrf_cookie, make_session_cookie,
    SessionStore,
};
use crate::App;

/// Serve the login page HTML.
pub async fn render_login(next: Option<&str>) -> http::Result<Response<Full<Buf>>> {
    let next_val = next.unwrap_or("");
    let html = LOGIN_HTML
        .replace("{{next}}", next_val)
        .replace("{{error}}", "");
    ok_html(html.into_bytes())
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
        let pair = pair.replace("%40", "@");
        if let Some((k, v)) = pair.split_once('=') {
            match k {
                "username" => form_username = urlencoding::decode(v).unwrap_or_default().to_string(),
                "password" => form_password = urlencoding::decode(v).unwrap_or_default().to_string(),
                "next" => form_next = urlencoding::decode(v).unwrap_or_default().to_string(),
                _ => {}
            }
        }
    }

    if form_username == app.config.admin.username && form_password == app.config.admin.password {
        let (token, csrf) = sessions.create_session(&form_username).await;
        let redirect_to = if form_next.is_empty() {
            "/admin/".to_string()
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

    let error_html = r#"<p class="text-red-400 text-sm mb-4 font-medium">Invalid username or password</p>"#;
    let body = LOGIN_HTML
        .replace("{{next}}", "")
        .replace("{{error}}", error_html);
    ok_html(body.into_bytes())
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
        .header("Location", "/admin/login")
        .header("Set-Cookie", make_logout_cookie())
        .header("Set-Cookie", make_logout_csrf_cookie())
        .body(Full::new(Buf::from("Redirecting...")))
        .unwrap();
    Ok(resp)
}

fn ok_html(body: Vec<u8>) -> http::Result<Response<Full<Buf>>> {
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Buf::from(body)))
        .unwrap())
}

const LOGIN_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Sign in — Pangolin</title>
<link href="/admin/app.css" rel="stylesheet">
<script src="https://unpkg.com/htmx.org@1.9.0"></script>
<style>
@import url('https://fonts.googleapis.com/css2?family=Inter:wght@400;500;600;700&display=swap');
</style>
</head>
<body class="bg-pangolin-slate min-h-screen flex items-center justify-center px-4">
  <div class="w-full max-w-sm">
    <!-- Logo mark -->
    <div class="flex justify-center mb-6">
      <div class="w-16 h-16 rounded-2xl bg-gradient-to-br from-brand-500 to-pangolin-accent flex items-center justify-center shadow-2xl shadow-brand-500/30">
        <svg class="w-9 h-9 text-white" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5">
          <path d="M12 2L2 7l10 5 10-5-10-5z"/>
          <path d="M2 17l10 5 10-5"/>
          <path d="M2 12l10 5 10-5"/>
        </svg>
      </div>
    </div>
    <div class="text-center mb-8">
      <h1 class="text-2xl font-bold text-white">Pangolin</h1>
      <p class="text-pangolin-silver text-sm mt-1">Sign in to your admin panel</p>
    </div>

    <form method="POST" action="/admin/login" class="bg-pangolin-steel rounded-2xl p-6 shadow-2xl border border-pangolin-zinc">
      {{error}}

      <div class="mb-4">
        <label class="block text-pangolin-silver text-sm mb-1.5" for="username">Username</label>
        <input type="text" name="username" id="username" required autocomplete="username"
          class="w-full bg-slate-800 border border-slate-600 rounded-lg px-4 py-2.5 text-white placeholder-slate-500
                 focus:outline-none focus:border-brand-500 focus:ring-1 focus:ring-brand-500 transition-colors"
          placeholder="admin">
      </div>

      <div class="mb-6">
        <label class="block text-pangolin-silver text-sm mb-1.5" for="password">Password</label>
        <input type="password" name="password" id="password" required autocomplete="current-password"
          class="w-full bg-slate-800 border border-slate-600 rounded-lg px-4 py-2.5 text-white placeholder-slate-500
                 focus:outline-none focus:border-brand-500 focus:ring-1 focus:ring-brand-500 transition-colors"
          placeholder="••••••••">
      </div>

      <button type="submit"
        class="w-full bg-brand-600 hover:bg-brand-700 active:bg-brand-800 text-white font-semibold
               rounded-lg px-4 py-2.5 transition-colors cursor-pointer">
        Sign in
      </button>
    </form>
  </div>
</body>
</html>"#;