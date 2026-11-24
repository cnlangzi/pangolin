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
//!
//! ## The two roles (issue #234)
//!
//! The gateway runs as **two** processes: a resident `supervisor`
//! and the `generation` that serves the traffic. The supervisor
//! starts the first generation, watches it and replaces it when the
//! operator asks for a reload (`SIGUSR2`); the generation does the
//! work and drains away when it is replaced.
//!
//! The role is decided by the arguments: `--generation` (hidden) is
//! the generation, everything else is the supervisor. See
//! `crate::reload` for the process model and the handover.

mod acme;
mod api_service;
mod badbots_feed;
mod check;
mod dns;
mod provider_ip;
mod proxy;
mod reload;
mod reuseport;
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clap::Parser;
use pingora::listeners::TcpSocketOptions;
use pingora::proxy::http_proxy_service;
use pingora::server::{RunArgs, Server};
use pingora::services::listening::Service;

use pangolin_core::config::Config;
use tokio_util::sync::CancellationToken;

// ---- CLI entry point ----

/// Where the gateway's database lives.
///
/// Relative to the working directory, exactly as it has always been:
/// the packaged service runs with `/var/lib/pangolin` as its working
/// directory, the Ansible deployment with `/usr/local/pangolin`.
pub(crate) const DB_PATH: &str = "pangolin.db";

#[derive(Parser, Debug)]
#[command(name = "ngx")]
#[command(about = "Pangolin gateway — public-facing HTTP/WebSocket proxy")]
// The release workflow asserts that `--version` reports the tag the
// binary was built from (see docs/release.md), so the string is the
// workspace version behind a `v` — the same shape as the release tag
// and the published archive name.
#[command(version = concat!("v", env!("CARGO_PKG_VERSION")))]
struct Args {
    /// Path to config file (default: ./ngx.yml)
    #[arg(short, long, default_value = "ngx.yml")]
    config: PathBuf,

    /// Check the configuration and exit, without serving anything.
    ///
    /// The check parses the configuration, opens the database,
    /// verifies the cert dir and builds the SNI callback — the steps
    /// a real start goes through — and then stops. It binds no port,
    /// starts no service, installs no signal handler and writes no
    /// PID file.
    ///
    /// It is **not** a read-only operation: it applies the pending
    /// database migrations, resets the certificate rows a dead
    /// process left in `Issuing` and reconciles the certificates on
    /// disk into the `certs` table — exactly as a real start would.
    /// The point is that the check and the start follow the same
    /// path.
    ///
    /// The database is the one in the **current working directory**
    /// (`pangolin.db`), so run the check from the directory the
    /// service runs in (the packaged service: `/var/lib/pangolin`).
    #[arg(long)]
    check: bool,

    /// Run as the generation of a supervisor.
    ///
    /// The supervisor starts a generation with this flag; it is not
    /// meant for an operator. A generation without a supervisor
    /// refuses to start — it would hold the ports with nobody to
    /// replace it.
    #[arg(long, hide = true)]
    generation: bool,

    /// Ask the running gateway to hand over to a new generation.
    ///
    /// The command sends `SIGUSR2` to the supervisor and exits; the
    /// handover itself is asynchronous. The gateway has to be running
    /// — the command finds it through the PID file.
    #[arg(long, conflicts_with_all = ["check", "status", "generation"])]
    reload: bool,

    /// Report the state of the running gateway.
    ///
    /// Prints the supervisor's process number and the generation's
    /// (with its status: `ok` or `draining`).
    #[arg(long, conflicts_with_all = ["check", "reload", "generation"])]
    status: bool,
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

    // ---- 1. The arguments --------------------------------------------------
    let args = Args::parse();

    // The operator commands address the running gateway and exit.
    // They install **no** logger: their answer belongs on stdout and
    // stderr, not in the log file — the same rule the `--check`
    // follows.
    if args.reload {
        return crate::reload::trigger_reload(&args.config);
    }
    if args.status {
        return crate::reload::report_status(&args.config);
    }

    // The configuration and the process logger are set up together:
    // the retired-key warnings need the logger, and a `log::warn!`
    // emitted before the logger exists would be swallowed by the
    // `log` crate's no-op logger — the operator would never learn
    // which key is gone.
    // (see `referer_log_e2e::the_referer_log_records_even_when_the_config_carries_the_retired_key`)
    //
    // `--check` is the exception: it deliberately does **not** install
    // the logger, so that a successful check says nothing at all.
    let config = if args.check {
        pangolin_core::config::Config::from_file(&args.config)
            .map_err(|e| anyhow::anyhow!("config error: {}", e))?
    } else {
        pangolin_core::load_config_and_init_logger(&args.config)
            .map_err(|e| anyhow::anyhow!("config error: {}", e))?
    };

    // The offline check (issue #233): it runs the validation steps of
    // the real startup and stops before anything is bound.
    if args.check {
        return crate::check::run_check(config);
    }

    // The role (issue #234). Without `--generation` this process is
    // the supervisor: it starts the first generation and watches it.
    if !args.generation {
        return crate::reload::supervise(&args.config, config);
    }

    serve_generation(config)
}

/// Run one generation of the gateway.
///
/// The generation is the child of the supervisor: it binds the
/// listeners, opens the database and serves the traffic. It never
/// starts a generation of its own — that is the supervisor's job.
fn serve_generation(config: Config) -> anyhow::Result<()> {
    // A generation has to have a supervisor: an orphan would hold the
    // ports with nobody to replace it.
    let supervisor = crate::reload::require_supervisor()?;
    // The kernel kills the generation when its supervisor dies, so a
    // `SIGKILL`ed supervisor does not leave a generation behind
    // holding the ports.
    crate::reload::arm_parent_death(supervisor)?;
    if !crate::reload::supervisor_is_parent(supervisor) {
        anyhow::bail!(
            "the supervisor {supervisor} is not this process's parent — it died during the handover"
        );
    }

    // The readiness pipe, if this generation is the incoming one of a
    // handover. The first generation of a fresh start has no outgoing
    // generation to hand over to, so it gets no pipe.
    let ready = crate::reload::ReadySignal::from_env()?;
    let is_handover = ready.is_some();

    let db_path = PathBuf::from(DB_PATH);
    let cert_manager = cert_manager(&config);
    let app = Arc::new(pangolin_core::App::new(
        &db_path,
        config.clone(),
        cert_manager,
    )?);

    log::info!(
        "Pangolin ngx {} generation starting on http={} https={}",
        pangolin_core::VERSION,
        config.addr.http,
        config.addr.https,
    );

    // Ensure the cert dir exists before any service starts.
    let cert_dir = std::path::PathBuf::from(&app.config.acme.cert_dir);
    if !cert_dir.exists() {
        std::fs::create_dir_all(&cert_dir)?;
    }

    // ---- Build shared shutdown token ------------------------------------
    let shutdown = CancellationToken::new();
    // A startup failure has nothing to drain: pingora's grace period
    // would only delay the error the supervisor is waiting for.
    let quick_shutdown = Arc::new(AtomicBool::new(false));

    // ---- Build the ACME state. The actual DNS reload + initial
    //         cert scan run inside `AcmeService::run`, so a startup
    //         failure there fails the process (fail-fast). The
    //         `CertRetrier` bridge that wires admin's `POST /certs/retry`
    //         to this state is installed inside the host runtime below.
    let acme_state = Arc::new(crate::acme::AcmeState::empty());

    // The ports the handover has to see bound before it may replace
    // the running generation (issue #234).
    let listener_ports = crate::reload::listener_ports(&config);
    // The budget of the drain. The tunnel sessions are closed within
    // it, and pingora's grace period is set to it.
    let drain_budget = Duration::from_secs(config.reload.drain_timeout_secs);
    let tunnel_drain_budget = drain_budget.min(crate::tunnel::TUNNEL_DRAIN_BUDGET);
    // The budget the incoming generation gives its own listeners to
    // come up.
    let ready_budget = Duration::from_secs(config.reload.ready_timeout_secs);

    // ---- Spawn pingora on its own std::thread ---------------------------
    // pingora runs `server.run(args)`, which observes our
    // `CancellationToken` via `TokenShutdownSignalWatch` for graceful
    // shutdown. The thread closure owns its `App` and shutdown clones.
    let pingora_thread = {
        let app = app.clone();
        let shutdown = shutdown.clone();
        let quick = quick_shutdown.clone();
        let config = config.clone();
        std::thread::Builder::new()
            .name("pangolin-pingora".to_string())
            .spawn(move || run_pingora(app, config, shutdown, quick))?
    };

    // ---- Host runtime: signals + non-pingora services -------------------
    // tunnel.addr is a full host:port string (default 0.0.0.0:9001);
    // operators set it to e.g. 127.0.0.1:9001 to keep tun clients on
    // this host only.
    let tunnel_addr = config.tunnel.addr.clone();
    // The QUIC listener binds the **same port number** as the
    // WebSocket/TCP tunnel listener, over UDP. TCP and UDP have
    // separate port spaces, so no new configuration key is needed.
    // A failure to bind it is not fatal: the gateway logs a warning
    // and keeps serving the WebSocket/TCP tunnel.
    let tunnel_quic_addr: std::net::SocketAddr = tunnel_addr
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid tunnel server address {tunnel_addr:?}: {e}"))?;
    // The QUIC certificate lives with the rest of the gateway's key
    // material, so a restart does not change it (the tun pins its
    // fingerprint).
    let tunnel_quic_cert_dir =
        pangolin_tun::certificate_dir(std::path::Path::new(&config.acme.cert_dir));

    // The shutdown token has to be cancellable from outside the host
    // runtime: a failure in the readiness handshake has to stop
    // pingora as well.
    let shutdown_outside = shutdown.clone();
    let quick_outside = quick_shutdown.clone();
    let quick_in_host = quick_outside.clone();

    let host_result = runtime::block_on_host(async move {
        // OS signal handlers cancel the shared token.
        {
            let app_for_signal = app.clone();
            runtime::install_signal_handlers(shutdown.clone(), move || {
                // SIGUSR1: rebuild the in-memory indexes from the
                // database. The supervisor sends it once the
                // outgoing generation is gone, so the writes that
                // landed on the outgoing one are in the database.
                let app = app_for_signal.clone();
                tokio::spawn(async move {
                    app.reload_indexes().await;
                });
            });
        }

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
        let quic_service: Box<dyn runtime::Service> = if is_handover {
            // A generation that has to report itself as ready may not
            // come up without its primary transport: a silent
            // downgrade would push every tunnel onto the fallback
            // for good, and the handover would look like it worked.
            Box::new(tunnel::quic::QuicService::new_strict(
                tunnel_quic_addr,
                tunnel_quic_cert_dir,
            ))
        } else {
            Box::new(tunnel::quic::QuicService::new(
                tunnel_quic_addr,
                tunnel_quic_cert_dir,
            ))
        };
        let services: Vec<Box<dyn runtime::Service>> = vec![
            Box::new(acme::AcmeService::new(acme_for_service)),
            Box::new(tunnel::TunnelService::new(tunnel_addr)),
            // The QUIC tunnel endpoint — the primary transport.
            quic_service,
            // Issue #122: keep the providers' published source-IP
            // ranges fresh — the origin guard compares the TCP peer
            // against them.
            Box::new(crate::provider_ip::ProviderIpService::new(
                std::time::Duration::from_secs(app.config.provider.ip_refresh_interval_secs),
            )?),
            // Issue #141: keep the reputation library's published
            // lists fresh. The service is registered unconditionally
            // and does nothing while the library is absent — the
            // operator can turn the check on from the dashboard, and
            // the service picks the new instance up on its next tick.
            Box::new(crate::badbots_feed::BadbotsFeedService::new()),
            // Issue #185: the dashboard-facing management API — its
            // own listener, its own namespace, its own authentication.
            Box::new(crate::api_service::ApiService::new(
                config.api.listen.clone(),
            )),
        ];

        // `ServiceContext` takes ownership of the `App`; keep a
        // clone so we can still drive the bot-log writer's graceful
        // shutdown after `drain_services` returns.
        let app_for_shutdown = app.clone();
        let ctx = runtime::ServiceContext::new(app, shutdown.clone());

        // Spawn the bot-log JSONL writer task. Must happen
        // **inside** the tokio runtime — `App::new` runs from
        // synchronous `main()` before the runtime exists, so
        // it can't spawn the task itself. Without this call, the
        // bot-side-channel `bot-YYYY-MM-DD.jsonl` file is never
        // written in production (a bug fixed in the post-merge
        // review). Returns `true` on success; we ignore the bool
        // and let `shutdown_bot_writer` be a silent no-op when
        // disabled (so the call is always safe).
        app_for_shutdown.start_bot_writer();

        // Spawn the referer-log JSONL writer task. Same reasoning as
        // the bot writer above: `App::new` runs before the runtime
        // exists, so the task has to be spawned from inside it.
        // The referer log is always on (issue #227), so the call is
        // safe either way.
        app_for_shutdown.start_referer_writer();

        let mut handles = Vec::with_capacity(services.len());
        for svc in services.into_iter() {
            handles.push(runtime::spawn_service(svc, ctx.clone()));
        }

        // The readiness handshake (issue #234). The incoming
        // generation says it is ready once **every** listener is
        // bound — and only then: a generation that came up with a
        // missing listener would replace a working gateway with a
        // broken one.
        if let Some(ready) = &ready {
            // The handshake must not ignore a service that failed:
            // a service that could not bind cancels the shared
            // token, and the handover has to fail rather than
            // replace a working gateway with a broken one.
            let missing = tokio::select! {
                missing = crate::reload::wait_for_listeners(&listener_ports, ready_budget) => missing?,
                _ = ctx.shutdown.cancelled() => {
                    // Nothing to drain: this generation never served
                    // a request, so pingora does not have to wait out
                    // its grace period.
                    quick_in_host.store(true, Ordering::Release);
                    anyhow::bail!("a service failed to start — refusing to take over");
                }
            };
            if !missing.is_empty() {
                // Nothing to drain: this generation never served a
                // request, so pingora does not have to wait out its
                // grace period.
                quick_in_host.store(true, Ordering::Release);
                anyhow::bail!("the listeners {missing:?} are not bound — refusing to take over");
            }
            ready
                .signal_ready()
                .map_err(|e| anyhow::anyhow!("could not report readiness: {e}"))?;
            log::info!("the generation is ready");
        }

        // Block on shutdown.
        ctx.shutdown.cancelled().await;
        log::info!("shutdown signalled, draining host services");

        // A: the accept loops have stopped. The listeners take no
        // new connections any more — the new connections go to the
        // incoming generation.
        let failure = runtime::drain_services(handles, drain_budget).await;

        // B: close the tunnel sessions, so the tun reconnects to the
        // incoming generation instead of waiting for this one to go.
        crate::tunnel::close_sessions_for_drain(tunnel_drain_budget).await;

        // Bot log JSONL writer flush. After `drain_services` the
        // pingora worker has stopped emitting access log entries, so
        // the bot fan-out (driven by `App::push_access_log`) has gone
        // quiet. Signal the background writer to perform a final
        // drain and `await` its completion (Gap #5/#6 fix — was
        // previously fire-and-forget).
        app_for_shutdown.shutdown_bot_writer().await;
        // Referer log JSONL writer flush — same reasoning as the bot
        // writer: the last referral rows must reach the disk before
        // the process exits.
        app_for_shutdown.shutdown_referer_writer().await;
        // Ask the traffic aggregator to exit on its next tick.
        // Join happens when `App` drops, not on the host runtime.
        app_for_shutdown.traffic.shutdown();

        // A service that could not bind a listener is a failure of
        // the process: it has to say so with a non-zero exit code,
        // or a handover would take a broken generation for a working
        // one.
        if let Some(e) = failure {
            return Err(e);
        }
        Ok::<(), anyhow::Error>(())
    });

    // A failure in the host runtime (a listener that could not bind)
    // has to stop pingora too, or the process would hang on the join.
    if host_result.is_err() {
        quick_outside.store(true, Ordering::Release);
        shutdown_outside.cancel();
    }

    // ---- Wait for pingora to finish ------------------------------------
    if let Err(e) = pingora_thread.join() {
        log::warn!("pingora thread panicked: {:?}", e);
    }

    host_result
}

/// The certificate manager the gateway runs with.
///
/// One place owns the construction: the real startup and the offline
/// check (`--check`) have to agree on it, or the check would validate
/// a different setup than the one that ends up serving.
pub(crate) fn cert_manager(config: &Config) -> CertManager {
    CertManager::new(
        PathBuf::from(&config.acme.cert_dir),
        config.acme.email.clone(),
        config.acme.acme_directory.clone(),
        config.acme.renew_threshold_days,
        config.acme.renew_check_interval_hours,
        config.acme.key_type.clone(),
    )
}

/// Build the pingora `Server`, add the proxy + admin services, and
/// run it with a `TokenShutdownSignalWatch` so a single shared
/// shutdown token drives the whole process.
fn run_pingora(
    app: Arc<App>,
    config: Config,
    shutdown: CancellationToken,
    quick_shutdown: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    // pingora's default `grace_period_seconds` is 300s (5 minutes) —
    // far too long for a Ctrl-C exit. It is the **drain budget** of
    // the configuration (issue #234): the generation waits this long
    // for the in-flight requests before pingora shuts its runtimes
    // down. The per-runtime shutdown timeout stays at 5s, so the
    // whole drain is the budget plus five seconds.
    //
    // The budget has to stay below the `TimeoutStopSec` of the
    // systemd unit (30 seconds), or systemd kills the generation in
    // the middle of its drain.
    let conf = pingora::server::configuration::ServerConf {
        grace_period_seconds: Some(config.reload.drain_timeout_secs),
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

    // The socket options every listener of the gateway shares.
    //
    // Every listener asks for `SO_REUSEPORT` (issue #233): two
    // generations of the gateway have to be able to hold the same port
    // at the same time, so that the outgoing one can drain while the
    // incoming one already serves. The kernel spreads the connections
    // over the sockets of the group by the hash of each connection's
    // 4-tuple.
    let mut sock_opt = TcpSocketOptions::default();
    sock_opt.so_reuseport = Some(true);

    // HTTP proxy service (domain-routed).
    let mut proxy_service = http_proxy_service(&conf, app_proxy);
    proxy_service.add_tcp_with_settings(&config.addr.http, sock_opt.clone());
    if config.addr.https != ":0" && !config.addr.https.is_empty() {
        // v2: SNI callback that loads per-host cert blobs on demand from
        // config.acme.cert_dir. The previous static-blob path with
        // "default" fallback was removed.
        //
        // fix/cert_www: SNI resolution is now driven by a pre-computed
        // domain → cert link (`app.cert_links`), so wildcard certs
        // (e.g. *.example.com) cover their subdomains at handshake
        // time. See docs/design/cert-link.md.
        //
        // ALPN is decided per-SNI inside `build_sni_settings` — the
        // listener runs a dynamic callback that follows
        // `config.tls.enable_h2` for direct sites and for tunnel sites
        // on the QUIC transport, and picks h1 for a tunnel site on the
        // WebSocket/yamux fallback.
        // See `tls::install_dynamic_alpn` for the policy and
        // `pangolin_core::App::tunnel_classification` for the data it
        // consults.
        let cert_dir = std::path::PathBuf::from(&app.config.acme.cert_dir);
        let tls_settings = crate::tls::build_sni_settings(cert_dir.clone(), app.clone())?;
        proxy_service.add_tls_with_settings(
            &config.addr.https,
            Some(sock_opt.clone()),
            tls_settings,
        );
        log::info!(
            "TLS enabled (SNI, dynamic ALPN) on {} (cert_dir: {}, enable_h2: {})",
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
    http_server.add_tcp_with_settings(&config.admin.addr, sock_opt);
    server.add_service(http_server);

    // Drive pingora with our shared shutdown token.
    let run_args = RunArgs {
        shutdown_signal: Box::new(runtime::TokenShutdownSignalWatch {
            token: shutdown,
            quick: quick_shutdown,
        }),
    };
    server.run(run_args);
    log::info!("pingora exited cleanly");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// The release workflow gates the upload on `--version` printing
    /// the tagged version (issue #128), so the `v` prefix and the
    /// workspace version must stay in lockstep. See
    /// `cli_version_is_workspace_version_with_v_prefix` in
    /// crates/tun/src/main.rs — both binaries answer the same way.
    #[test]
    fn cli_version_is_workspace_version_with_v_prefix() {
        let cmd = Args::command();
        assert_eq!(
            cmd.get_version(),
            Some(concat!("v", env!("CARGO_PKG_VERSION")))
        );
    }

    /// The workflow matches the pushed tag against
    /// `^v(\d+\.\d+\.\d+…)$` and then asserts the binary reports
    /// `v<tag version>` — so the reported version has to be a
    /// `v`-prefixed semver, not merely a string containing a `v`.
    #[test]
    fn cli_version_is_a_v_prefixed_semver() {
        let cmd = Args::command();
        let version = cmd
            .get_version()
            .expect("the clap command declares a version");
        let core = version
            .strip_prefix('v')
            .expect("the version starts with 'v'")
            .split(['-', '+'])
            .next()
            .unwrap_or_default();
        let parts: Vec<&str> = core.split('.').collect();
        assert_eq!(
            parts.len(),
            3,
            "version '{version}' is not major.minor.patch"
        );
        assert!(
            parts
                .iter()
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())),
            "version '{version}' has a non-numeric component"
        );
    }
}
