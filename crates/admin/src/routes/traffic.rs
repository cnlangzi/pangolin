//! `/traffic` — in-memory Network / Request stats.
//!
//! Page is server-rendered from [`App::traffic_snapshot`]. HTMX
//! polls `/api/traffic/kpis` (2s) and `/api/traffic/tables` (8s).
//! `POST /traffic/reset` (CSRF) asks the aggregator to zero
//! counters without touching in-flight atomics.

use std::sync::Arc;

use askama::Template;
use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use crate::App;
use crate::ok_html_with_csrf;
use crate::routes::helpers::{ok_html, redirect};
use crate::templates::{TrafficKpisView, TrafficPageTemplate, TrafficTablesView};

pub async fn render(app: &Arc<App>, csrf: &str) -> http::Result<Response<Full<Bytes>>> {
    let snap = (*app.traffic_snapshot()).clone();
    let spark_bars = snap.spark_bars();
    let tmpl = TrafficPageTemplate {
        snap,
        spark_bars,
        csrf_token: csrf.to_string(),
        active_nav: "traffic",
    };
    let html = match tmpl.render() {
        Ok(s) => s,
        Err(e) => {
            log::error!("traffic template error: {e}");
            String::new()
        }
    };
    ok_html_with_csrf(html, csrf)
}

pub async fn api_kpis(app: &Arc<App>) -> http::Result<Response<Full<Bytes>>> {
    let snap = (*app.traffic_snapshot()).clone();
    let spark_bars = snap.spark_bars();
    let html = TrafficKpisView { snap, spark_bars }
        .render()
        .unwrap_or_default();
    ok_html(html)
}

pub async fn api_tables(app: &Arc<App>) -> http::Result<Response<Full<Bytes>>> {
    let snap = (*app.traffic_snapshot()).clone();
    let html = TrafficTablesView { snap }.render().unwrap_or_default();
    ok_html(html)
}

pub async fn handle_reset(app: &Arc<App>) -> http::Result<Response<Full<Bytes>>> {
    app.traffic.request_reset();
    Ok(redirect("/traffic"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pangolin_core::CertManager;
    use pangolin_core::config::Config;
    use tempfile::TempDir;

    fn make_test_app() -> (TempDir, Arc<App>) {
        let dir = TempDir::new().expect("tempdir");
        let db_path = dir.path().join("test.db");
        let app = Arc::new(
            App::new(
                db_path.to_str().expect("utf8"),
                Config::default(),
                CertManager::default(),
            )
            .expect("App::new"),
        );
        (dir, app)
    }

    #[tokio::test]
    async fn traffic_page_renders_title() {
        let (_dir, app) = make_test_app();
        let resp = render(&app, "csrf").await.expect("render");
        assert_eq!(resp.status(), 200);
        let snap = (*app.traffic_snapshot()).clone();
        let html = TrafficPageTemplate {
            spark_bars: snap.spark_bars(),
            snap,
            csrf_token: "csrf".into(),
            active_nav: "traffic",
        }
        .render()
        .expect("template");
        assert!(html.contains("Traffic"), "page title: {html}");
        assert!(html.contains("/api/traffic/kpis"), "{html}");
        assert!(
            html.contains("In-memory") || html.contains("in-memory"),
            "{html}"
        );
    }

    #[tokio::test]
    async fn reset_redirects() {
        let (_dir, app) = make_test_app();
        let resp = handle_reset(&app).await.expect("reset");
        assert_eq!(resp.status(), 302);
        let loc = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(loc, "/traffic");
    }
}
