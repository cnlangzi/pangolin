//! Process-level runtime: Service trait, signal handling, and pingora
//! shutdown coordination.
//!
//! The gateway is composed of several long-running services that share
//! an `Arc<App>`:
//!
//! * `AcmeService`     — periodic cert renewal + initial scan
//! * `TunnelService`   — WebSocket listener for tun nodes
//! * `PingoraService`  — HTTP proxy + admin API (runs on a dedicated
//!   std::thread because pingora owns its own tokio runtime; we don't
//!   try to integrate it with our host runtime)
//!
//! All non-pingora services run on a single host tokio runtime and
//! observe a shared [`CancellationToken`] for coordinated shutdown.
//! The same token is plugged into pingora via
//! [`TokenShutdownSignalWatch`] so a single signal stops the whole
//! process.

use std::future::Future;
use std::sync::Arc;
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

/// A long-running service managed by the host runtime.
#[async_trait]
pub trait Service: Send + Sync + 'static {
    /// Stable name for logging.
    fn name(&self) -> &'static str;
    /// Drive the service until `ctx.shutdown` is cancelled. Returning
    /// `Err` aborts process startup (fail-fast).
    async fn run(&self, ctx: ServiceContext) -> anyhow::Result<()>;
}

/// Build the multi-thread host runtime and run `f` to completion on it.
pub fn block_on_host<F>(f: F) -> F::Output
where
    F: Future,
{
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("pangolin-host")
        .build()
        .expect("failed to build host tokio runtime");
    rt.block_on(f)
}

/// How long to wait for graceful drain before giving up on a service.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(15);

/// Install OS signal handlers that cancel `token`. The handler tasks
/// themselves live on the host runtime; this function must be called
/// from inside a `block_on_host` future.
///
/// Replaces pingora's default `UnixShutdownSignalWatch` so there is
/// exactly one set of signal handlers in the process.
pub fn install_signal_handlers(token: CancellationToken) {
    // SIGINT (Unix) / Ctrl-C (Windows) — fast shutdown
    let token_for_ctrl_c = token.clone();
    tokio::spawn(async move {
        if let Err(e) = tokio::signal::ctrl_c().await {
            error!("ctrl_c handler: {e}");
            return;
        }
        info!("ctrl_c received, cancelling shutdown token");
        token_for_ctrl_c.cancel();
    });

    // SIGTERM (Unix only) — graceful terminate
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let token_for_sigterm = token.clone();
        tokio::spawn(async move {
            match signal(SignalKind::terminate()) {
                Ok(mut s) => {
                    s.recv().await;
                    info!("SIGTERM received, cancelling shutdown token");
                    token_for_sigterm.cancel();
                }
                Err(e) => error!("SIGTERM handler: {e}"),
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
}

#[async_trait]
impl ShutdownSignalWatch for TokenShutdownSignalWatch {
    async fn recv(&self) -> ShutdownSignal {
        self.token.cancelled().await;
        info!("shutdown token cancelled, signalling pingora to terminate");
        ShutdownSignal::GracefulTerminate
    }
}

/// Spawn `svc.run(ctx)` on the current (host) runtime and return the
/// join handle. The service is consumed because the spawned task
/// requires `'static`.
pub fn spawn_service(
    svc: Box<dyn Service>,
    ctx: ServiceContext,
) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    let name = svc.name();
    info!("starting service: {name}");
    tokio::spawn(async move {
        if let Err(e) = svc.run(ctx).await {
            warn!("service {name} returned error: {e}");
        }
        Ok(())
    })
}

/// Drain a list of service join handles. The first one to error or
/// the drain deadline terminates the loop; the rest are abandoned
/// (the process is exiting anyway).
pub async fn drain_services(handles: Vec<tokio::task::JoinHandle<anyhow::Result<()>>>) {
    if handles.is_empty() {
        return;
    }
    let deadline = tokio::time::sleep(DRAIN_TIMEOUT);
    tokio::pin!(deadline);
    for h in handles {
        tokio::select! {
            r = h => match r {
                Ok(Ok(())) => {}
                Ok(Err(e)) => warn!("service returned error: {e}"),
                Err(e) => warn!("service join error: {e}"),
            },
            _ = &mut deadline => {
                warn!("drain deadline reached; abandoning remaining services");
                return;
            }
        }
    }
}
