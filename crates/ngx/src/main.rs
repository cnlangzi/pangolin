//! Pangolin gateway (ngx) — public-facing HTTP/WebSocket proxy.
//!
//! Two services share the same `Arc<pangolin_core::App>`:
//!   - HTTP proxy via `http_proxy_service` + `impl ProxyHttp` for domain-routed proxying
//!   - HTTP server via `HttpServer::new_app` + `impl ServeHttp` for admin API + static files

mod acme;
mod admin_api;
mod proxy;
mod serve;
mod tunnel;

pub use proxy::AppProxy;
pub use serve::AppHttp;

// Bring the external admin UI crate into the bin's crate root so that
// `crate::admin::...` works inside `serve` and other bin-only modules.
// (Unused here in main, but referenced via `crate::admin::*` in `serve`.)
#[allow(unused_imports)]
use ::admin;

// Re-export App/TunnelMessage/CertManager for internal modules that expect them in crate namespace
pub use pangolin_core::{App, CertManager, TunnelMessage};

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use pingora::apps::http_app::HttpServer;
use pingora::proxy::http_proxy_service;
use pingora::server::Server;
use pingora::services::listening::Service;

use pangolin_core::config::Config;

// ---- CLI entry point ----

#[derive(Parser, Debug)]
#[command(name = "ngx")]
#[command(about = "Pangolin gateway — public-facing HTTP/WebSocket proxy")]
struct Args {
    /// Path to config file (default: ./pangolin.toml)
    #[arg(short, long, default_value = "pangolin.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = Args::parse();
    let config =
        Config::from_file(&args.config).map_err(|e| anyhow::anyhow!("config error: {}", e))?;

    let db_path = PathBuf::from("pangolin.db");
    let cert_manager = CertManager {
        enabled: config.cert.autorenew,
        cert_dir: PathBuf::from(&config.cert.cert_dir),
        email: config.cert.email.clone(),
        acme_directory: config.cert.acme_directory.clone(),
        renew_threshold_days: config.cert.renew_threshold_days,
        renew_check_interval_hours: config.cert.renew_check_interval_hours,
        renew_max_retries: config.cert.renew_max_retries,
    };
    let app = Arc::new(pangolin_core::App::new(
        &db_path,
        config.clone(),
        cert_manager,
    )?);

    log::info!(
        "Pangolin ngx {} starting on port {}",
        pangolin_core::VERSION,
        config.server.port
    );

    // Build pingora server
    let mut server = Server::new(None)?;
    server.bootstrap();

    let conf = server.configuration.clone();
    let app_proxy = AppProxy { app: app.clone() };
    let app_http = AppHttp {
        app: app.clone(),
        sessions: Arc::new(::admin::state::SessionStore::default()),
    };

    // HTTP proxy service (domain-routed)
    let mut proxy_service = http_proxy_service(&conf, app_proxy);
    proxy_service.add_tcp(&format!("0.0.0.0:{}", config.server.port));
    if config.server.tls_port > 0 {
        let tls_addr = format!("0.0.0.0:{}", config.server.tls_port);
        let host = config.server.host.as_deref().unwrap_or("default");
        let (cert_path, key_path) = app
            .cert_manager
            .resolve_cert(host)
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        proxy_service.add_tls(&tls_addr, &cert_path, &key_path)?;
        log::info!("TLS enabled on {}", tls_addr);
    }
    server.add_service(proxy_service);

    // HTTP server (admin API + static files)
    let http_server: Service<_> =
        Service::new("pangolin-http".to_string(), HttpServer::new_app(app_http));
    server.add_service(http_server);

    // Tunnel WebSocket server (independent TCP listener, runs as background task)
    let app_tunnel = app.clone();
    let tunnel_addr = format!("127.0.0.1:{}", config.server.tunnel_port);
    tokio::spawn(async move {
        tunnel::start_tunnel_server(app_tunnel, &tunnel_addr).await;
    });

    server.run_forever();
}
