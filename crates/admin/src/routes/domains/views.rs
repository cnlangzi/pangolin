//! Domains HTMX partials.
//!
//! The only HTMX endpoint under `/api/...` for domains is
//! `GET /api/site/{name}/domains`, which returns the per-site table
//! for HTMX swap. Now uses askama template rendering to maintain
//! consistency with the full page view.

use std::sync::Arc;

use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use askama::Template;

use crate::templates::SiteDomainsTableView;
use crate::App;

type Resp = Response<Full<Bytes>>;

fn ok_html(body: String) -> http::Result<Resp> {
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .unwrap())
}

/// Render the per-site domains table for HTMX swap.
/// Endpoint: `GET /api/site/{name}/domains`.
///
/// Uses the askama template `views/domains/_site_table.html` to ensure
/// styling consistency with the full page view at `/site/{name}/domains`.
pub async fn render_table_for_site(
    app: &Arc<App>,
    site_name: &str,
    csrf: &str,
) -> http::Result<Resp> {
    let db = app.db.lock().await;
    let all_domains = pangolin_core::db::list_domains(&db).unwrap_or_default();
    drop(db);
    let site_name_owned = site_name.to_owned();
    let domains: Vec<_> = all_domains
        .into_iter()
        .filter(|d| d.site_name == site_name_owned)
        .collect();

    let html = SiteDomainsTableView {
        domains,
        site_name,
        active_nav: "domains",
    }
    .render()
    .unwrap();

    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}
