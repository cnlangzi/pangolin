//! HTTP server via pingora `ServeHttp` trait for admin API + static files.
//!
//! This runs as a separate pingora Service sharing the same App state.

use std::sync::Arc;

use async_trait::async_trait;
use log::debug;
use pingora::apps::http_app::ServeHttp;
use pingora::http::ResponseHeader;
use pingora::protocols::http::ServerSession;

use crate::{admin, App};

/// `ServeHttp` implementation for the admin API + static file serving.
pub struct AppHttp {
    pub app: Arc<App>,
}

#[async_trait]
impl ServeHttp for AppHttp {
    async fn response(&self, http_session: &mut ServerSession) -> http::Response<Vec<u8>> {
        let req = http_session.req_header();
        let path = req.uri.path().to_string();
        let method = req.method.as_str().to_string();

        // Health check
        if path == "/health" || path == "/ping" {
            let mut resp = ResponseHeader::build(200, None).unwrap();
            resp.insert_header("Content-Type", "text/plain").ok();
            let _ = http_session.write_response_header(Box::new(resp)).await;
            let _ = http_session
                .write_response_body(bytes::Bytes::new(), true)
                .await;
            return http::Response::builder().status(200).body(vec![]).unwrap();
        }

        // Admin API
        if path.starts_with("/api/") {
            debug!("HTTP admin API: {} {}", method, path);
            return admin::handle_api_http(http_session, &self.app, &path, &method).await;
        }

        // Root
        if path == "/" {
            let body = b"Pangolin ngx running".to_vec();
            return http::Response::builder()
                .status(200)
                .header("Content-Type", "text/plain")
                .body(body)
                .unwrap();
        }

        // 404
        let mut resp = ResponseHeader::build(404, None).unwrap();
        resp.insert_header("Content-Type", "text/plain").ok();
        let _ = http_session.write_response_header(Box::new(resp)).await;
        let _ = http_session
            .write_response_body(bytes::Bytes::from_static(b"Not found"), true)
            .await;
        http::Response::builder()
            .status(404)
            .header("Content-Type", "text/plain")
            .body(b"Not found".to_vec())
            .unwrap()
    }
}
