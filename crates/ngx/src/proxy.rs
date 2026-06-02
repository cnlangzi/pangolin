//! HTTP proxy via pingora `ProxyHttp` trait.
//!
//! `AppProxy` implements `ProxyHttp` and handles domain-routed proxying.
//! `request_filter` short-circuits for admin API / tunnel routes.
//! Otherwise falls through to `upstream_peer` for direct backends.

use std::sync::Arc;

use async_trait::async_trait;
use log::{debug, error, info, warn};
use pingora::http::{RequestHeader, ResponseHeader};
use pingora::proxy::{ProxyHttp, Session};
use pingora::upstreams::peer::HttpPeer;
use pingora_core::prelude::*;

use crate::{admin, App, TunnelMessage};

/// `ProxyHttp` implementation for pangolin.
pub struct AppProxy {
    pub app: Arc<App>,
}

#[async_trait]
impl ProxyHttp for AppProxy {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    /// Request filter — short-circuit for admin API, static files, or tunnel routes.
    ///
    /// Returns `Ok(true)` if we handled the response locally (no upstream proxy).
    /// Returns `Ok(false)` to continue to `upstream_peer`.
    async fn request_filter(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<bool> {
        let path = session.req_header().uri.path().to_string();
        let method = session.req_header().method.as_str().to_string();

        // Admin API: short-circuit, don't proxy
        if path.starts_with("/api/") {
            debug!("Admin API request: {}", path);
            admin::handle_api_request(session, &self.app, &path, &method).await?;
            return Ok(true);
        }

        // WebSocket tunnel path: handle via tunnel
        if path == self.app.ws_path {
            info!("WebSocket tunnel request, upgrading connection");
            // TODO: handle WS upgrade to tunnel handler
            session.respond_error(426).await;
            return Ok(true);
        }

        // Look up site by Host header
        let host = session
            .get_header("Host")
            .and_then(|v| std::str::from_utf8(v.as_bytes()).ok())
            .unwrap_or("");

        let indexes = self.app.indexes.read().await;
        let site = match pangolin_core::index::lookup_site(&indexes, host) {
            Some(s) => s.clone(),
            None => {
                debug!("No site found for host: {}", host);
                session.respond_error(404).await;
                return Ok(true);
            }
        };
        drop(indexes);

        // Parse the backend to determine routing type
        let backend_str = site.backend.clone();
        let (tun_name, url) = match pangolin_core::parse::parse_backend(&backend_str) {
            Ok((t, u)) => (t, u),
            Err(e) => {
                error!("Invalid backend for site {}: {}", site.name, e);
                session.respond_error(502).await;
                return Ok(true);
            }
        };

        // Tunnel path: forward request to the live tun session
        if !tun_name.is_empty() {
            let sender = {
                let sessions = self.app.tun_sessions.read().await;
                sessions.get(&tun_name).cloned()
            };
            if let Some(sender) = sender {
                debug!("Tunnel routing: {} → tun {}", host, tun_name);
                let rid = format!(
                    "req-{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                );
                let body = format!("{} {}\nHost: {}", method, path, host);
                let msg = TunnelMessage {
                    rid,
                    body: body.into_bytes(),
                    last: true,
                };
                if sender.send(msg).await.is_err() {
                    warn!("Tun {} disconnected", tun_name);
                    session.respond_error(503).await;
                } else {
                    session.respond_error(200).await;
                }
                return Ok(true);
            } else {
                warn!("Tun {} not online", tun_name);
                session.respond_error(503).await;
                return Ok(true);
            }
        }

        // Direct path: continue to upstream_peer (return Ok(false))
        debug!("Direct proxy: {} → {}", host, url);
        Ok(false)
    }

    /// Select the upstream peer based on the site backend URL.
    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let host = session
            .get_header("Host")
            .and_then(|v| std::str::from_utf8(v.as_bytes()).ok())
            .unwrap_or("");

        let indexes = self.app.indexes.read().await;
        let site = match pangolin_core::index::lookup_site(&indexes, host) {
            Some(s) => s.clone(),
            None => {
                error!("No site for host: {}", host);
                return Err(Error::new_str("site not found"));
            }
        };
        drop(indexes);

        let url = match pangolin_core::parse::parse_backend(&site.backend) {
            Ok((_, u)) => u,
            Err(e) => {
                return Err(Error::explain(ErrorType::ReadError, format!("bad backend: {}", e)));
            }
        };

        // Determine TLS and address based on scheme
        let (address, tls, sni) = if url.starts_with("https://") {
            let addr = url.trim_start_matches("https://");
            let port_sep = addr.find(':').unwrap_or(addr.len());
            let host_part = &addr[..port_sep];
            let port: u16 = addr[port_sep + 1..]
                .trim_start_matches(':')
                .parse()
                .unwrap_or(443);
            (format!("{}:{}", host_part, port), true, host_part.to_string())
        } else if url.starts_with("http://") {
            let addr = url.trim_start_matches("http://");
            let port_sep = addr.find(':').unwrap_or(addr.len());
            let host_part = &addr[..port_sep];
            let port: u16 = addr[port_sep + 1..]
                .trim_start_matches(':')
                .parse()
                .unwrap_or(80);
            (format!("{}:{}", host_part, port), false, String::new())
        } else {
            return Err(Error::new_str("unsupported backend scheme"));
        };

        let peer = HttpPeer::new(address, tls, sni);
        Ok(Box::new(peer))
    }

    /// Preserve original Host header for upstream (important for vhosting).
    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream: &mut RequestHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        if let Some(host) = session.get_header("Host") {
            upstream.insert_header("Host", host).ok();
        }
        Ok(())
    }

    /// Response filter — could add headers or log here.
    async fn response_filter(
        &self,
        _session: &mut Session,
        _upstream_response: &mut ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        Ok(())
    }
}