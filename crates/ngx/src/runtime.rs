//! Process-level runtime: Service trait, signal handling, and pingora
//! shutdown coordination.
//!
//! The gateway is composed of several long-running services that share
//! an `Arc<App>`:
//!
//! * `AcmeService`   — periodic cert renewal + initial scan
//! * `TunnelService` — WebSocket listener for tun nodes
//! * `AdminService`  — the admin UI's own HTTP listener (issue #261)
//! * `DashboardService` — the outbound channel to the dashboard
//! * pingora (driven by `run_pingora` in `main.rs`) — the HTTP proxy.
//!   pingora owns its own tokio runtime, so it lives on a dedicated
//!   `std::thread` rather than the host runtime.
//!
//! All non-pingora services run on a single host tokio runtime and
//! observe a shared [`CancellationToken`] for coordinated shutdown.
//! The same token is plugged into pingora via
//! [`TokenShutdownSignalWatch`] so a single signal stops the whole
//! process.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use log::{error, info, warn};
use pangolin_core::App;
use pingora::server::{ShutdownSignal, ShutdownSignalWatch};
use tokio_util::sync::CancellationToken;

/// Per-service context handed to every long-running component.
///
/// Cloning is cheap (both fields are `Arc` / shared).
#[derive(Clone)]
pub struct ServiceContext {
    pub app: Arc<App>,
    pub shutdown: CancellationToken,
}

impl ServiceContext {
    pub fn new(app: Arc<App>, shutdown: CancellationToken) -> Self {
        Self { app, shutdown }
    }
}

/// Whether this process is draining.
///
/// A process that is draining has stopped accepting new connections
/// but is still finishing the requests it took — the admin's
/// `/healthz` says so (issue #234).
///
/// The flag lives here rather than in the reload module because the
/// health endpoint needs it, and the health endpoint is part of the
/// library the integration tests use.
static DRAINING: AtomicBool = AtomicBool::new(false);

/// Whether this process is draining.
pub fn is_draining() -> bool {
    DRAINING.load(Ordering::Acquire)
}

/// Mark this process as draining.
///
/// Called from the signal handler, **before** the shutdown token is
/// cancelled: the health endpoint has to answer "draining" before
/// the listeners stop answering at all.
pub fn mark_draining() {
    DRAINING.store(true, Ordering::Release);
}

/// A long-running service managed by the host runtime.
#[async_trait]
pub trait Service: Send + Sync + 'static {
    /// Stable name for logging.
    fn name(&self) -> &'static str;
    /// Drive the service until `ctx.shutdown` is cancelled. Returning
    /// `Err` aborts process startup (fail-fast).
    async fn run(&self, ctx: ServiceContext) -> anyhow::Result<()>;
}

/// Build the host runtime and run `f` to completion on it.
///
/// The host runtime drives the long-lived non-pingora services (the
/// ACME renewal loop, the tunnel accept loop, the admin UI's
/// listener, the outbound dashboard channel) plus a couple of signal
/// handler tasks. The work is dominated by network I/O, so two worker
/// threads is plenty — `num_cpus` (the `new_multi_thread` default)
/// would just park a herd of idle OS threads.
///
/// The admin UI's requests are the one thing here that is not pure
/// I/O: rendering a page reads the database. That work runs on the
/// blocking pool (issue #254), so it does not occupy a worker
/// thread; the two threads carry the accept loops and the streams.
pub fn block_on_host<F>(f: F) -> F::Output
where
    F: Future,
{
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("pangolin-host")
        .build()
        .expect("failed to build host tokio runtime");
    rt.block_on(f)
}

/// How long to wait for graceful drain before giving up on a service.
///
/// The **deadline**, not the budget: the services observe the
/// shutdown token and return at once, so this only matters when one
/// of them is wedged. The budget of the drain itself is the
/// configuration's `[reload] drain_timeout_secs` — the deadline is
/// clamped to it, so the whole drain fits into the budget the
/// operator configured.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(15);

/// Install OS signal handlers that cancel `token`. The handler tasks
/// themselves live on the host runtime; this function must be called
/// from inside a `block_on_host` future.
///
/// Replaces pingora's default `UnixShutdownSignalWatch` so there is
/// exactly one set of signal handlers in the process.
///
/// The signals and what they mean (issue #234):
///
/// - `SIGINT` / `SIGTERM` — the process is asked to stop. The token
///   is cancelled and everything drains.
/// - `SIGUSR2` — the **generation** is asked to hand over. The token
///   is cancelled as well, and the generation marks itself as
///   draining first, so the health endpoint says "draining" instead
///   of "ok".
/// - `SIGUSR1` — the generation is asked to rebuild its indexes. The
///   supervisor sends it once the outgoing generation is gone, so the
///   writes that landed on the outgoing one are in the database.
///
/// The supervisor has its own handlers — it is a different process
/// with a different job (see `crate::reload`).
pub fn install_signal_handlers(
    token: CancellationToken,
    on_reload_indexes: impl Fn() + Send + Sync + 'static,
) {
    // SIGINT (Unix) / Ctrl-C (Windows) — fast shutdown
    let token_for_ctrl_c = token.clone();
    tokio::spawn(async move {
        if let Err(e) = tokio::signal::ctrl_c().await {
            error!("ctrl_c handler: {e}");
            return;
        }
        info!("ctrl_c received, cancelling shutdown token");
        mark_draining();
        token_for_ctrl_c.cancel();
    });

    // SIGTERM (Unix only) — graceful terminate
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let token_for_sigterm = token.clone();
        tokio::spawn(async move {
            match signal(SignalKind::terminate()) {
                Ok(mut s) => {
                    s.recv().await;
                    info!("SIGTERM received, cancelling shutdown token");
                    mark_draining();
                    token_for_sigterm.cancel();
                }
                Err(e) => error!("SIGTERM handler: {e}"),
            }
        });

        // SIGUSR2 — the generation is asked to hand over. The
        // supervisor sends it once the incoming generation has
        // reported that it is ready.
        let token_for_sigusr2 = token.clone();
        tokio::spawn(async move {
            match signal(SignalKind::user_defined2()) {
                Ok(mut s) => {
                    s.recv().await;
                    info!("SIGUSR2 received — draining for the handover");
                    mark_draining();
                    token_for_sigusr2.cancel();
                }
                Err(e) => error!("SIGUSR2 handler: {e}"),
            }
        });

        // SIGUSR1 — rebuild the in-memory indexes from the database.
        // The supervisor sends it after the outgoing generation is
        // gone: the writes that landed on the outgoing one are in
        // the database by then.
        tokio::spawn(async move {
            match signal(SignalKind::user_defined1()) {
                Ok(mut s) => {
                    s.recv().await;
                    info!("SIGUSR1 received — rebuilding the indexes");
                    on_reload_indexes();
                }
                Err(e) => error!("SIGUSR1 handler: {e}"),
            }
        });
    }
}

/// A pingora `ShutdownSignalWatch` that observes a `CancellationToken`
/// and asks pingora to gracefully terminate.
///
/// `ShutdownSignal::GracefulTerminate` mirrors what
/// `UnixShutdownSignalWatch` would emit on SIGTERM, which is the
/// desired behaviour for `Ctrl-C` / `kill <pid>` as well.
pub struct TokenShutdownSignalWatch {
    pub token: CancellationToken,
    /// Whether the shutdown has to be quick.
    ///
    /// A startup failure has nothing to drain — the generation never
    /// served a request — so pingora's grace period would only
    /// delay the error the supervisor is waiting for.
    pub quick: Arc<AtomicBool>,
}

#[async_trait]
impl ShutdownSignalWatch for TokenShutdownSignalWatch {
    async fn recv(&self) -> ShutdownSignal {
        self.token.cancelled().await;
        info!("shutdown token cancelled, signalling pingora to terminate");
        if self.quick.load(Ordering::Acquire) {
            ShutdownSignal::FastShutdown
        } else {
            ShutdownSignal::GracefulTerminate
        }
    }
}

/// Spawn `svc.run(ctx)` on the current (host) runtime and return the
/// join handle. The service is consumed because the spawned task
/// requires `'static`.
///
/// If the service returns `Err`, the shared shutdown token is
/// cancelled so the failure cascades through the existing shutdown
/// plumbing (host drain + pingora's `TokenShutdownSignalWatch`).
/// The error is logged and the join handle resolves to `Ok(())` —
/// the caller in `main` is expected to check that the process
/// actually shut down cleanly (it will, because of the cascade).
pub fn spawn_service(
    svc: Box<dyn Service>,
    ctx: ServiceContext,
) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    let name = svc.name();
    info!("starting service: {name}");
    let shutdown = ctx.shutdown.clone();
    tokio::spawn(async move {
        // The error is **returned**, not only logged: a service that
        // could not bind its listener is a failure of the process,
        // and the process has to say so with a non-zero exit code.
        // The handover watches for it — a generation that came up
        // without a listener would look ready, and the supervisor
        // would drain a working gateway for a broken one.
        match svc.run(ctx).await {
            Ok(()) => Ok(()),
            Err(e) => {
                warn!("service {name} returned error: {e}; cancelling shutdown");
                shutdown.cancel();
                Err(e)
            }
        }
    })
}

/// Drain a list of service join handles. The first one to error or
/// the drain deadline terminates the loop; the rest are abandoned
/// (the process is exiting anyway).
///
/// The first error is returned: a service that could not bind a
/// listener is a failure of the **process**, and the process has to
/// say so with a non-zero exit code. A handover watches for it —
/// a generation that came up without a listener would look ready.
pub async fn drain_services(
    handles: Vec<tokio::task::JoinHandle<anyhow::Result<()>>>,
    budget: Duration,
) -> Option<anyhow::Error> {
    if handles.is_empty() {
        return None;
    }
    let mut failure: Option<anyhow::Error> = None;
    let deadline = tokio::time::sleep(DRAIN_TIMEOUT.min(budget));
    tokio::pin!(deadline);
    for h in handles {
        tokio::select! {
            r = h => match r {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    warn!("service returned error: {e}");
                    failure.get_or_insert(e);
                }
                Err(e) => warn!("service join error: {e}"),
            },
            _ = &mut deadline => {
                warn!("drain deadline reached; abandoning remaining services");
                return failure;
            }
        }
    }
    failure
}
