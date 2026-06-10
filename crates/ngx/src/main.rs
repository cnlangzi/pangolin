//! Pangolin gateway (ngx) — public-facing HTTP/WebSocket proxy.
//!
//! Startup architecture (see also `runtime.rs`):
//!
//! 1. `main` is a synchronous function. It performs all blocking
//!    initialization (config, DB, `App`, cert dir, `AcmeState`).
//! 2. A single multi-thread tokio runtime ("host") is created and
//!    drives:
//!      - OS signal handlers (cancel a shared `CancellationToken`).
//!      - `AcmeService`  — periodic cert renewal + initial scan.
//!      - `TunnelService` — WebSocket listener for tun nodes.
//! 3. pingora is built and run on a dedicated `std::thread`.
//!    pingora cannot live on the host runtime because it owns its
//!    own tokio runtime. Its shutdown is driven by the same
//!    `CancellationToken` via `TokenShutdownSignalWatch`, so a single
//!    SIGINT/SIGTERM stops the whole process.
//! 4. After shutdown, host services drain, the host runtime
//!    finishes, the pingora thread is joined, and `main` returns.

mod acme;
mod admin_api;
mod dns;
mod proxy;
mod runtime;
mod serve;
mod tls;
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
use pingora::server::{RunArgs, Server};
use pingora::services::listening::Service;

use pangolin_core::config::Config;
use tokio_util::sync::CancellationToken;

// ---- CLI entry point ----

#[derive(Parser, Debug)]
#[command(name = "ngx")]
#[command(about = "Pangolin gateway — public-facing HTTP/WebSocket proxy")]
struct Args {
    /// Path to config file (default: ./pangolin.yml)
    #[arg(short, long, default_value = "pangolin.yml")]
    config: PathBuf,
}

// DNS providers in v2 are stored in the `dns_providers` SQLite table and
// loaded into the App's in-memory index at startup; the per-domain
// `dns_provider` column on `domains` decides which provider to use at
// issuance time. The `build_dns_provider` helper from PR #20 (which read
// the global `cert.dns.*` YAML section) has been removed; the equivalent
// wiring now lives in PR-2 (issuance pipeline) under `App::dns_providers`.

fn main() -> anyhow::Result<()> {
    // ---- 1. Blocking init --------------------------------------------------
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = Args::parse();
    let config =
        Config::from_file(&args.config).map_err(|e| anyhow::anyhow!("config error: {}", e))?;

    let db_path = PathBuf::from("pangolin.db");
    let cert_manager = CertManager::new(
        PathBuf::from(&config.acme.cert_dir),
        config.acme.email.clone(),
        config.acme.acme_directory.clone(),
        config.acme.renew_threshold_days,
        config.acme.renew_check_interval_hours,
        config.acme.renew_max_retries,
        config.acme.key_type.clone(),
    );
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

    // Ensure the cert dir exists before any service starts.
    let cert_dir = std::path::PathBuf::from(&app.config.acme.cert_dir);
    if !cert_dir.exists() {
        std::fs::create_dir_all(&cert_dir)?;
    }

    // ---- 2. Build shared shutdown token ------------------------------------
    let shutdown = CancellationToken::new();

    // ---- 3. Build the ACME state. The actual DNS reload + initial
    //         cert scan run inside `AcmeService::run`, so a startup
    //         failure there fails the process (fail-fast).
    let acme_state = Arc::new(crate::acme::AcmeState::empty());

    // ---- 4. Build services -------------------------------------------------
    let tunnel_addr = config.server.tunnel_port.to_string();

    // ---- 5. Spawn pingora on its own std::thread ---------------------------
    // pingora runs `server.run(args)`, which observes our
    // `CancellationToken` via `TokenShutdownSignalWatch` for graceful
    // shutdown. The thread closure owns its `App` and shutdown clones.
    let pingora_thread = {
        let app = app.clone();
        let shutdown = shutdown.clone();
        let config = config.clone();
        std::thread::Builder::new()
            .name("pangolin-pingora".to_string())
            .spawn(move || run_pingora(app, config, shutdown))?
    };

    // ---- 6. Host runtime: signals + non-pingora services -------------------
    let host_result = runtime::block_on_host(async move {
        // OS signal handlers cancel the shared token.
        runtime::install_signal_handlers(shutdown.clone());

        // Build & start services (fail-fast on startup error).
        let services: Vec<Box<dyn runtime::Service>> = vec![
            Box::new(acme::AcmeService::new(acme_state)),
            Box::new(tunnel::TunnelService::new(format!(
                "127.0.0.1:{tunnel_addr}"
            ))),
        ];

        let ctx = runtime::ServiceContext::new(app, shutdown.clone());

        let mut handles = Vec::with_capacity(services.len());
        for svc in services.into_iter() {
            handles.push(runtime::spawn_service(svc, ctx.clone()));
        }

        // Block on shutdown.
        ctx.shutdown.cancelled().await;
        log::info!("shutdown signalled, draining host services");

        runtime::drain_services(handles).await;
        Ok::<(), anyhow::Error>(())
    });

    // ---- 7. Wait for pingora to finish ------------------------------------
    if let Err(e) = pingora_thread.join() {
        log::warn!("pingora thread panicked: {:?}", e);
    }

    host_result
}

/// Build the pingora `Server`, add the proxy + admin services, and
/// run it with a `TokenShutdownSignalWatch` so a single shared
/// shutdown token drives the whole process.
fn run_pingora(app: Arc<App>, config: Config, shutdown: CancellationToken) -> anyhow::Result<()> {
    let mut server = Server::new(None)?;
    server.bootstrap();

    let conf = server.configuration.clone();
    let app_proxy = AppProxy { app: app.clone() };
    let app_http = AppHttp {
        app: app.clone(),
        sessions: Arc::new(::admin::state::SessionStore::default()),
    };

    // HTTP proxy service (domain-routed).
    let mut proxy_service = http_proxy_service(&conf, app_proxy);
    proxy_service.add_tcp(&format!("0.0.0.0:{}", config.server.port));
    if config.server.tls_port > 0 {
        let tls_addr = format!("0.0.0.0:{}", config.server.tls_port);
        // v2: SNI callback that loads per-host cert blobs on demand.
        let cert_dir = std::path::PathBuf::from(&app.config.acme.cert_dir);
        let tls_settings = crate::tls::build_sni_settings(cert_dir.clone())?;
        proxy_service.add_tls_with_settings(&tls_addr, None, tls_settings);
        log::info!(
            "TLS enabled (SNI) with HTTP/2 ALPN on {} (cert_dir: {})",
            tls_addr,
            cert_dir.display()
        );
    }
    server.add_service(proxy_service);

    // Admin HTTP server (admin API + static files).
    let mut http_server = Service::new("pangolin-http".to_string(), HttpServer::new_app(app_http));
    http_server.add_tcp(&config.admin.addr);
    server.add_service(http_server);

    // Drive pingora with our shared shutdown token.
    let run_args = RunArgs {
        shutdown_signal: Box::new(runtime::TokenShutdownSignalWatch { token: shutdown }),
    };
    server.run(run_args);
    log::info!("pingora exited cleanly");
    Ok(())
}
