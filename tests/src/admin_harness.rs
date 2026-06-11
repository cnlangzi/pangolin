//! Admin UI test harness — wraps NgxProcess with login/session helpers.

use std::time::Duration;

use reqwest::{Client, Response};
use scraper::{Html, Selector};

use crate::harness::NgxProcess;

/// Per-request timeout for every AdminClient call. The local run completes
/// in well under 1s per request; on contended CI runners we've seen
/// individual requests hang for minutes (one 30s request, multiplied
/// across a parallel test runner, can blow past the whole e2e budget).
/// 10s is generous for a single in-process handler and short enough
/// that one slow test fails fast instead of blocking the whole suite.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Admin test client with session management.
pub struct AdminClient {
    client: Client,
    base_url: String,
}

impl AdminClient {
    /// Build the shared reqwest client used by `AdminClient`.
    /// Public so tests that need a raw client (e.g. unauth redirect
    /// tests) don't have to duplicate this builder.
    pub fn build_http_client() -> Client {
        Client::builder()
            .cookie_store(true)
            .danger_accept_invalid_certs(true)
            // Bound every request — a hung TCP connect or read would
            // otherwise keep the test thread alive past the e2e
            // job's overall timeout.
            .connect_timeout(Duration::from_secs(5))
            .timeout(REQUEST_TIMEOUT)
            // Don't auto-follow redirects — login (and most POSTs) need
            // to observe the 302 status, not transparently follow it.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("build reqwest client")
    }

    /// Create a new admin client for the given ngx process.
    pub fn new(ngx: &NgxProcess) -> Self {
        let client = Self::build_http_client();
        Self {
            client,
            base_url: format!("http://127.0.0.1:{}", ngx.admin_port),
        }
    }

    /// Login with username/password. Stores session cookie in the client.
    pub async fn login(&self, username: &str, password: &str) -> anyhow::Result<()> {
        // GET login page to get initial state (not strictly needed, but realistic)
        let login_page = self
            .client
            .get(&format!("{}/login", self.base_url))
            .send()
            .await?;

        if !login_page.status().is_success() {
            anyhow::bail!("GET /login returned {}", login_page.status());
        }

        // POST credentials
        let resp = self
            .client
            .post(&format!("{}/login", self.base_url))
            .form(&[("username", username), ("password", password)])
            .send()
            .await?;

        // Expect 302 redirect on success
        if resp.status().as_u16() != 302 {
            anyhow::bail!("Login failed: expected 302, got {}", resp.status());
        }

        Ok(())
    }

    /// GET an admin path. Returns the response.
    pub async fn get(&self, path: &str) -> anyhow::Result<Response> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self.client.get(&url).send().await?;
        Ok(resp)
    }

    /// POST to an admin path with form data.
    pub async fn post_form(&self, path: &str, form: &[(&str, &str)]) -> anyhow::Result<Response> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self.client.post(&url).form(&form).send().await?;
        Ok(resp)
    }

    /// PUT to an admin path with form data.
    pub async fn put_form(&self, path: &str, form: &[(&str, &str)]) -> anyhow::Result<Response> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self.client.put(&url).form(&form).send().await?;
        Ok(resp)
    }

    /// DELETE an admin path with query parameters.
    pub async fn delete(&self, path: &str, query: &[(&str, &str)]) -> anyhow::Result<Response> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self.client.delete(&url).query(&query).send().await?;
        Ok(resp)
    }

    /// DELETE an admin path with form data sent in the request body.
    /// This is the pattern used by HTMX hx-vals with hx-delete.
    pub async fn delete_form(&self, path: &str, form: &[(&str, &str)]) -> anyhow::Result<Response> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self.client.delete(&url).form(&form).send().await?;
        Ok(resp)
    }

    /// Extract CSRF token from HTML (looks for input[name="_csrf"]).
    pub fn csrf_token(&self, html: &str) -> Option<String> {
        let doc = Html::parse_document(html);
        let selector = Selector::parse(r#"input[name="_csrf"]"#).unwrap();
        doc.select(&selector)
            .next()
            .and_then(|el| el.value().attr("value"))
            .map(|s| s.to_string())
    }

    /// Assert that a CSS selector matches an element containing the given text.
    pub fn assert_contains(&self, html: &str, selector: &str, text: &str) -> anyhow::Result<()> {
        let doc = Html::parse_document(html);
        let sel = Selector::parse(selector)
            .map_err(|e| anyhow::anyhow!("Invalid selector '{}': {:?}", selector, e))?;

        let found = doc
            .select(&sel)
            .any(|el| el.text().collect::<String>().contains(text));

        if !found {
            anyhow::bail!(
                "Selector '{}' did not match any element containing '{}'",
                selector,
                text
            );
        }

        Ok(())
    }

    /// Assert that a CSS selector matches at least one element.
    pub fn assert_selector_exists(&self, html: &str, selector: &str) -> anyhow::Result<()> {
        let doc = Html::parse_document(html);
        let sel = Selector::parse(selector)
            .map_err(|e| anyhow::anyhow!("Invalid selector '{}': {:?}", selector, e))?;

        if doc.select(&sel).next().is_none() {
            anyhow::bail!("Selector '{}' did not match any element", selector);
        }

        Ok(())
    }
}
