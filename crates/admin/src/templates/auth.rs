//! Auth templates — login page only (logout uses 302 redirect, no template).

use askama::Template;

/// Login page template. Stands alone (does not extend `layouts/base.html`)
/// because auth has no nav and uses Datastar for live form validation.
#[derive(Template)]
#[template(path = "pages/auth/login.html")]
pub struct LoginTemplate<'a> {
    pub next: &'a str,
    pub error: &'a str,
}
