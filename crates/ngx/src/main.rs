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
mod dns;
mod proxy;
mod runtime;
mod serve;
mod sse;
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
use pingora::proxy::http_proxy_service;
use pingora::server::{RunArgs, Server};
use pingora::services::listening::Service;

use pangolin_core::config::Config;
use pangolin_core::init_logger;
use tokio_util::sync::CancellationToken;

// ---- CLI entry point ----

#[derive(Parser, Debug)]
#[command(name = "ngx")]
#[command(about = "Pangolin gateway — public-facing HTTP/WebSocket proxy")]
struct Args {
    /// Path to config file (default: ./ngx.yml)
    #[arg(short, long, default_value = "ngx.yml")]
    config: PathBuf,
}

// DNS providers in v2 are stored in the `dns_providers` SQLite table and
// loaded into the App's in-memory index at startup; the per-domain
// `dns_provider` column on `domains` decides which provider to use at
// issuance time. The `build_dns_provider` helper from PR #20 (which read
// the global `cert.dns.*` YAML section) has been removed; the equivalent
// wiring now lives in PR-2 (issuance pipeline) under `App::dns_providers`.

fn main() -> anyhow::Result<()> {
    // ---- 0. Install the rustls crypto provider --------------------
    // rustls 0.23 refuses to construct any TLS config without a
    // process-level CryptoProvider. The first reqwest call (inside
    // instant-acme when registering a new ACME account) would
    // otherwise panic with the famously unhelpful
    //   "Could not automatically determine the process-level
    //    CryptoProvider from Rustls crate features."
    // Single helper lives in pangolin_core so the same line runs in
    // every binary + test harness; switching providers (aws-lc-rs
    // etc.) is then a one-line change.
    pangolin_core::install_crypto_provider();

    // ---- 1. Blocking init --------------------------------------------------
    let args = Args::parse();
    let config =
        Config::from_file(&args.config).map_err(|e| anyhow::anyhow!("config error: {}", e))?;
    // Now that config is loaded, honor its `[log]` section for the
    // process logger (shared with `tun`).
    init_logger(&config.log);

    let db_path = PathBuf::from("pangolin.db");
    let cert_manager = CertManager::new(
        PathBuf::from(&config.acme.cert_dir),
        config.acme.email.clone(),
        config.acme.acme_directory.clone(),
        config.acme.renew_threshold_days,
        config.acme.renew_check_interval_hours,
        config.acme.key_type.clone(),
    );
    let app = Arc::new(pangolin_core::App::new(
        &db_path,
        config.clone(),
        cert_manager,
    )?);

    log::info!(
        "Pangolin ngx {} starting on http={} https={}",
        pangolin_core::VERSION,
        config.addr.http,
        config.addr.https,
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
    //         failure there fails the process (fail-fast). The
    //         `CertRetrier` bridge that wires admin's `POST /certs/retry`
    //         to this state is installed inside the host runtime below.
    let acme_state = Arc::new(crate::acme::AcmeState::empty());

    // ---- 4. Spawn pingora on its own std::thread ---------------------------
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

    // ---- 5. Host runtime: signals + non-pingora services -------------------
    // tunnel.addr is a full host:port string (default 0.0.0.0:9001);
    // operators set it to e.g. 127.0.0.1:9001 to keep tun clients on
    // this host only.
    let tunnel_addr = config.tunnel.addr.clone();

    let host_result = runtime::block_on_host(async move {
        // OS signal handlers cancel the shared token.
        runtime::install_signal_handlers(shutdown.clone());

        // Wire admin's `POST /certs/retry` to the ACME state (issue #45).
        // Lives inside the host runtime so the trait-object's
        // `RwLock::write` does not need its own runtime to drive.
        // `AcmeService` takes ownership of the same Arc below — the
        // clone is just two `Arc::increment`s, not a duplicate state.
        let acme_for_service = acme_state.clone();
        acme_state.install_on(&app).await;

        // Reconcile `cert_dir` with the `certs` table. Disk is the
        // source of truth: every parseable cert blob is upserted into
        // the DB so a successful on-disk issuance always surfaces on
        // the dashboard, even if the DB row was previously stuck in
        // Failed / Pending / Skipped. Idempotent — rows that already
        // match the file are left alone.
        match crate::acme::scan_and_reconcile_blobs(&app).await {
            Ok(n) if n > 0 => log::info!("ACME: cert_dir reconciled, {} row(s) updated", n),
            Ok(_) => log::info!("ACME: cert_dir reconciliation complete, no changes"),
            Err(e) => log::warn!("ACME: cert_dir reconciliation failed: {}", e),
        }

        // Build & start services (fail-fast on startup error).
        let services: Vec<Box<dyn runtime::Service>> = vec![
            Box::new(acme::AcmeService::new(acme_for_service)),
            Box::new(tunnel::TunnelService::new(tunnel_addr)),
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

    // ---- 6. Wait for pingora to finish ------------------------------------
    if let Err(e) = pingora_thread.join() {
        log::warn!("pingora thread panicked: {:?}", e);
    }

    host_result
}

/// Build the pingora `Server`, add the proxy + admin services, and
/// run it with a `TokenShutdownSignalWatch` so a single shared
/// shutdown token drives the whole process.
fn run_pingora(app: Arc<App>, config: Config, shutdown: CancellationToken) -> anyhow::Result<()> {
    // pingora's default `grace_period_seconds` is 300s (5 minutes) —
    // far too long for a Ctrl-C exit. We shorten it to 5s and also
    // cap the per-runtime shutdown timeout to 5s. The services have
    // already drained by the time we get here (host runtime sees
    // shutdown.cancelled() and exits), so pingora only needs a
    // moment to drop its runtimes.
    let conf = pingora::server::configuration::ServerConf {
        grace_period_seconds: Some(5),
        graceful_shutdown_timeout_seconds: Some(5),
        ..Default::default()
    };
    let mut server = Server::new_with_opt_and_conf(None, conf);
    server.bootstrap();

    let conf = server.configuration.clone();
    let app_proxy = AppProxy { app: app.clone() };
    let app_http = AppHttp {
        app: app.clone(),
        sessions: Arc::new(::admin::state::SessionStore::default()),
    };

    // HTTP proxy service (domain-routed).
    let mut proxy_service = http_proxy_service(&conf, app_proxy);
    proxy_service.add_tcp(&config.addr.http);
    if config.addr.https != ":0" && !config.addr.https.is_empty() {
        // v2: SNI callback that loads per-host cert blobs on demand from
        // config.acme.cert_dir. The previous static-blob path with
        // "default" fallback was removed.
        //
        // ALPN is decided per-SNI inside `build_sni_settings` — the
        // listener runs a dynamic callback that picks h1 for tunnel
        // sites and follows `config.tls.enable_h2` for everything else.
        // See `tls::install_dynamic_alpn` for the policy and
        // `pangolin_core::App::tunnel_domains` for the data it consults.
        let cert_dir = std::path::PathBuf::from(&app.config.acme.cert_dir);
        let tls_settings = crate::tls::build_sni_settings(cert_dir.clone(), app.clone())?;
        proxy_service.add_tls_with_settings(&config.addr.https, None, tls_settings);
        log::info!(
            "TLS enabled (SNI, dynamic ALPN) on {} (cert_dir: {}, non_tunnel_h2: {})",
            config.addr.https,
            cert_dir.display(),
            config.tls.enable_h2
        );
    }
    server.add_service(proxy_service);

    // Admin HTTP server (admin API + static files).
    // Issue #73: `AppHttp` now implements `HttpServerApp` directly
    // so the `/api/logs/stream` SSE endpoint can chunk its writes
    // over a long-lived connection (ServeHttp materialises the
    // whole body before returning, which would defeat SSE).
    let mut http_server = Service::new("pangolin-http".to_string(), app_http);
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
