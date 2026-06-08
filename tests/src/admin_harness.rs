//! Admin UI test harness — wraps NgxProcess with login/session helpers.

use reqwest::{Client, Response};
use scraper::{Html, Selector};

use crate::harness::NgxProcess;

/// Admin test client with session management.
pub struct AdminClient {
    client: Client,
    base_url: String,
}

impl AdminClient {
    /// Create a new admin client for the given ngx process.
    pub fn new(ngx: &NgxProcess) -> Self {
        let client = Client::builder()
            .cookie_store(true)
            .danger_accept_invalid_certs(true)
            .build()
            .expect("build reqwest client");

        Self {
            client,
            base_url: format!("http://127.0.0.1:{}", ngx.admin_port),
        }
    }

    /// Login with username/password. Stores session cookie in the client.
    pub async fn login(&self, username: &str, password: &str) -> anyhow::Result<()> {
        // GET login page to get initial state (not strictly needed, but realistic)
        let login_page = self.client
            .get(&format!("{}/admin/login", self.base_url))
            .send()
            .await?;

        if !login_page.status().is_success() {
            anyhow::bail!("GET /admin/login returned {}", login_page.status());
        }

        // POST credentials
        let resp = self.client
            .post(&format!("{}/admin/login", self.base_url))
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

    /// DELETE an admin path with query params.
    pub async fn delete(&self, path: &str, query: &[(&str, &str)]) -> anyhow::Result<Response> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self.client
            .delete(&url)
            .query(&query)
            .send()
            .await?;
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

        let found = doc.select(&sel).any(|el| {
            el.text().collect::<String>().contains(text)
        });

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
