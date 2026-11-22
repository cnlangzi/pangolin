//! Supervision of the background JSONL writer tasks.
//!
//! The bot log and the referer log each drain a queue into a daily
//! JSONL file from a background tokio task. A panic in that task stops
//! the file from growing for the rest of the process lifetime, and
//! almost nothing says so: the panic reaches stderr as one anonymous
//! `tokio-runtime-worker panicked` line that does not name the side
//! channel, every in-memory view of the channel (the ring buffer, the
//! SSE fan-out, the counters) keeps looking healthy, and the only
//! accurate signal arrives hours later, when the process shuts down.
//!
//! A supervisor owns the writer task's `JoinHandle` and restarts the
//! task when it panics, so the file keeps growing without an operator
//! doing anything. The restart budget is bounded: a deterministic
//! panic must not fill the journal, so after `max_restarts` restarts
//! inside one rolling window the supervisor gives up, says so in the
//! log and on the admin panel, and the side channel stays dead until
//! the gateway restarts.
//!
//! ## The shutdown signal has exactly one consumer
//!
//! The writer task selects on its own `Notify` to learn that it must
//! flush and exit. `Notify::notify_one` stores a *single* permit, so
//! two consumers of the same notification would race for it and one
//! of them would never see the signal — the writer would then only
//! stop when the shutdown timeout fired. The supervisor therefore
//! never selects on the writer's notification: it uses the explicit
//! [`SupervisedTask::begin_shutdown`] flag instead, and the writer
//! keeps the notification to itself.
//!
//! See `docs/design/bot-log.md` and `docs/design/referer-log.md` for
//! the operator-facing description of the failure modes.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// The observable state of a supervised task.
///
/// Shared between the supervisor (which writes it) and the admin
/// panel (which reads it), so every field is an atomic.
pub struct SupervisedTask {
    /// The task is running right now.
    alive: AtomicBool,
    /// Restarts since the process started.
    restarts: AtomicU32,
    /// The supervisor gave up — the task is dead until the process restarts.
    gave_up: AtomicBool,
    /// The shutdown was requested; the supervisor must not restart.
    shutting_down: AtomicBool,
    /// Wakes the supervisor out of its backoff when the shutdown starts.
    ///
    /// The supervisor's own notification, not the writer's: the
    /// writer's shutdown permit has exactly one consumer (the
    /// writer), and a second consumer would steal it.
    shutdown_notify: tokio::sync::Notify,
}

impl SupervisedTask {
    /// Build the state of a task that has not run yet.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            alive: AtomicBool::new(false),
            restarts: AtomicU32::new(0),
            gave_up: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            shutdown_notify: tokio::sync::Notify::new(),
        })
    }

    /// The task is running right now.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// Restarts since the process started.
    ///
    /// Counts the restarts the supervisor actually performed, not the
    /// panics it saw: a task that panicked once and could not be
    /// restarted (the budget was spent) was never restarted.
    pub fn restart_count(&self) -> u32 {
        self.restarts.load(Ordering::Relaxed)
    }

    /// The supervisor gave up — the task is dead until the process restarts.
    pub fn gave_up(&self) -> bool {
        self.gave_up.load(Ordering::Relaxed)
    }

    /// Ask the supervisor to stop restarting. Set before the task is told to stop.
    pub fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        // Wake a supervisor that is sleeping in its backoff, so the
        // shutdown does not have to wait the backoff out.
        self.shutdown_notify.notify_one();
    }

    /// Whether the shutdown was requested.
    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    /// Record that the task is (not) running.
    fn set_alive(&self, alive: bool) {
        self.alive.store(alive, Ordering::Relaxed);
    }

    /// Record that the supervisor performed a restart.
    fn record_restart(&self) {
        self.restarts.fetch_add(1, Ordering::Relaxed);
    }

    /// Record that the supervisor gave up.
    fn set_gave_up(&self) {
        self.gave_up.store(true, Ordering::Relaxed);
    }

    /// Wait until the shutdown is requested.
    async fn shutdown_requested(&self) {
        self.shutdown_notify.notified().await;
    }
}

/// The tunables of the restart loop.
///
/// The defaults are what production uses; the tests override them so
/// they do not have to wait a minute.
#[derive(Clone, Debug)]
pub struct SupervisorConfig {
    /// Restarts allowed inside one rolling window.
    pub max_restarts: u32,
    /// A run at least this long resets the window.
    pub stable_run: Duration,
    /// The first restart waits this long; each further one doubles it.
    pub backoff: Duration,
    /// The backoff never exceeds this.
    pub max_backoff: Duration,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            max_restarts: 5,
            stable_run: Duration::from_secs(60),
            backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(2),
        }
    }
}

/// Plain data about a supervised writer, for the admin UI.
///
/// The panel has to tell "the side channel is dead" apart from "there
/// was no traffic today", so the state is more than a bool: it also
/// carries how often the writer was restarted and how many entries
/// the hot path had to drop.
#[derive(Clone, Debug, Default)]
pub struct WriterHealth {
    /// `false` when there is no writer at all (the side channel is
    /// disabled) — the panel then says the status is unavailable
    /// instead of claiming health.
    pub available: bool,
    /// The writer task is running right now.
    pub alive: bool,
    /// Restarts since the process started.
    pub restarts: u32,
    /// The supervisor gave up — the side channel is dead.
    pub failed: bool,
    /// Entries dropped because the queue was full.
    pub dropped: u64,
}

/// Clears the `alive` flag when the supervisor goes away.
///
/// The supervisor is supposed to end only after the writer exited or
/// after it gave up — but a task that is aborted, or a supervisor that
/// unwinds on a bug of its own, would leave the flag saying "the
/// writer is running" while nothing is running. The guard makes the
/// flag honest in every path.
struct AliveGuard(Arc<SupervisedTask>);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.set_alive(false);
    }
}

/// Run `make`'s future as a supervised task.
///
/// The future runs in its own tokio task, so a panic surfaces as
/// `Err(JoinError)` instead of unwinding through the supervisor.
///
/// The loop:
///
/// 1. spawn the task, remember when it started;
/// 2. wait for it —
///    - a clean exit means the task was told to stop: return, do not
///      restart it;
///    - a panic means the task died: restart it, unless the shutdown
///      was requested;
/// 3. a run that lasted at least `stable_run` resets the rolling
///    window, so an old failure does not spend the budget of a
///    healthy writer;
/// 4. when the budget is spent, give up: the side channel is dead
///    until the process restarts.
pub async fn supervise<F, Fut>(
    state: Arc<SupervisedTask>,
    name: &'static str,
    config: SupervisorConfig,
    make: F,
) where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let _alive = AliveGuard(state.clone());
    // Restarts inside the current rolling window. Reset when the
    // task survives long enough to be considered stable.
    let mut window_restarts: u32 = 0;
    let mut backoff = config.backoff;
    loop {
        if state.is_shutting_down() {
            return;
        }
        state.set_alive(true);
        let started = Instant::now();
        // The writer runs in a task of its own: that is what turns a
        // panic into a `JoinError` instead of an unwind through the
        // supervisor.
        let handle = tokio::spawn(make());
        let join_err = match handle.await {
            Ok(()) => {
                // The task exited on its own — the shutdown signal.
                state.set_alive(false);
                return;
            }
            Err(join_err) => join_err,
        };
        state.set_alive(false);
        if join_err.is_cancelled() {
            // The runtime cancelled the task (a runtime shutdown, a
            // `JoinHandle::abort`). That is not a panic, and
            // restarting into a runtime that is going away would
            // fail anyway.
            log::warn!("{name}: writer task was cancelled: {join_err}");
            return;
        }
        if state.is_shutting_down() {
            // The shutdown was requested while the task was dying:
            // do not resurrect it.
            return;
        }
        if started.elapsed() >= config.stable_run {
            // The task ran long enough to be considered healthy, so
            // the panic that just happened starts a new window.
            window_restarts = 0;
        }
        window_restarts += 1;
        if window_restarts > config.max_restarts {
            state.set_gave_up();
            log::error!(
                "{name}: writer task panicked {window_restarts} times in a row; \
                 giving up — this side channel is dead until the gateway restarts"
            );
            return;
        }
        log::error!(
            "{name}: writer task panicked ({join_err}); restarting \
             ({window_restarts}/{})",
            config.max_restarts
        );
        state.record_restart();
        // Wait before the next attempt, so a deterministic panic does
        // not spin the CPU — and so the disk gets a moment to come
        // back before the writer is asked to open the file again.
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = state.shutdown_requested() => {
                return;
            }
        }
        backoff = (backoff * 2).min(config.max_backoff);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// Wait until `check` holds, or give up after roughly a second.
    ///
    /// The supervisor runs as a task of its own, so the state it
    /// publishes lands a scheduling tick after the call that caused
    /// it — a test that asserted right away would be racing.
    async fn wait_for(check: impl Fn() -> bool) -> bool {
        for _ in 0..200 {
            if check() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        check()
    }

    /// A config whose timings are short enough for a unit test.
    fn fast_config(max_restarts: u32) -> SupervisorConfig {
        SupervisorConfig {
            max_restarts,
            stable_run: Duration::from_secs(60),
            backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(5),
        }
    }

    /// A task that panics on its first run and returns cleanly on the
    /// second one.
    #[tokio::test]
    async fn supervisor_restarts_a_task_that_panicked() {
        let state = SupervisedTask::new();
        let runs = Arc::new(AtomicUsize::new(0));

        let runs_for_task = runs.clone();
        let supervisor = tokio::spawn(supervise(
            state.clone(),
            "test",
            fast_config(5),
            move || {
                let runs = runs_for_task.clone();
                async move {
                    if runs.fetch_add(1, Ordering::SeqCst) == 0 {
                        panic!("the writer task panicked");
                    }
                }
            },
        ));

        assert!(
            wait_for(|| state.restart_count() == 1).await,
            "the supervisor must restart the task that panicked: \
             restarts = {}, alive = {}, gave_up = {}",
            state.restart_count(),
            state.is_alive(),
            state.gave_up()
        );
        assert!(
            !state.gave_up(),
            "one restart must not spend the whole budget"
        );
        assert_eq!(runs.load(Ordering::SeqCst), 2, "the task must run twice");

        // The second run returned cleanly, so the supervisor is done.
        supervisor.await.expect("supervisor joins");
        assert!(!state.is_alive(), "the task is gone once it returned");
    }

    /// A task that always panics spends the restart budget and then
    /// leaves the side channel dead.
    #[tokio::test]
    async fn supervisor_gives_up_after_five_restarts() {
        let state = SupervisedTask::new();
        let runs = Arc::new(AtomicUsize::new(0));

        let runs_for_task = runs.clone();
        let supervisor = tokio::spawn(supervise(
            state.clone(),
            "test",
            fast_config(5),
            move || {
                let runs = runs_for_task.clone();
                async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    panic!("the writer task panicked");
                }
            },
        ));

        supervisor.await.expect("supervisor joins");
        assert!(state.gave_up(), "the budget must be spent");
        assert!(!state.is_alive(), "a dead side channel is not alive");
        assert_eq!(
            state.restart_count(),
            5,
            "five restarts were performed before the supervisor gave up"
        );
        assert_eq!(
            runs.load(Ordering::SeqCst),
            6,
            "the task ran once and was restarted five times"
        );
    }

    /// A task that survives long enough is considered healthy again:
    /// its next panic starts a new window.
    #[tokio::test]
    async fn supervisor_resets_the_budget_after_a_stable_run() {
        let state = SupervisedTask::new();
        let runs = Arc::new(AtomicUsize::new(0));
        let config = SupervisorConfig {
            // One restart per window: without the reset, the second
            // panic would spend the budget and the supervisor would
            // give up instead of running the task a third time.
            max_restarts: 1,
            // Milliseconds, so the test does not have to wait a minute.
            stable_run: Duration::from_millis(50),
            backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(5),
        };

        let runs_for_task = runs.clone();
        let supervisor = tokio::spawn(supervise(state.clone(), "test", config, move || {
            let runs = runs_for_task.clone();
            async move {
                let run = runs.fetch_add(1, Ordering::SeqCst);
                if run == 1 {
                    // The second run is the stable one: it survives
                    // past `stable_run` and then panics.
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    panic!("the writer task panicked after a stable run");
                }
                if run == 0 {
                    panic!("the writer task panicked");
                }
            }
        }));

        // The stable run in between resets the window, so the third
        // run still happens and the supervisor did not give up.
        assert!(
            wait_for(|| runs.load(Ordering::SeqCst) == 3).await,
            "the task must run three times: panic, stable run, panic; runs = {}",
            runs.load(Ordering::SeqCst)
        );
        assert_eq!(
            state.restart_count(),
            2,
            "both panics must trigger a restart"
        );
        assert!(
            !state.gave_up(),
            "the stable run must have reset the budget"
        );
        state.begin_shutdown();
        let _ = supervisor.await;
    }

    /// A shutdown that arrives before the panic stops the supervisor
    /// from resurrecting the task.
    #[tokio::test]
    async fn supervisor_does_not_restart_after_shutdown() {
        let state = SupervisedTask::new();
        let runs = Arc::new(AtomicUsize::new(0));

        let runs_for_task = runs.clone();
        let supervisor = tokio::spawn(supervise(
            state.clone(),
            "test",
            fast_config(5),
            move || {
                let runs = runs_for_task.clone();
                async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    // Give the test time to ask for the shutdown.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    panic!("the writer task panicked");
                }
            },
        ));

        // The shutdown has to arrive while the task is running, so
        // that the panic really does race with it.
        assert!(
            wait_for(|| runs.load(Ordering::SeqCst) == 1).await,
            "the task must be running before the shutdown is requested"
        );
        state.begin_shutdown();
        supervisor.await.expect("supervisor joins");
        assert_eq!(
            state.restart_count(),
            0,
            "a shutdown must stop the supervisor from restarting"
        );
        assert!(!state.is_alive());
        assert!(!state.gave_up(), "a shutdown is not a failure");
        assert_eq!(runs.load(Ordering::SeqCst), 1, "the task ran once");
    }

    /// A task that exits cleanly is not restarted — that is what a
    /// graceful shutdown looks like from the supervisor's side.
    #[tokio::test]
    async fn supervisor_does_not_restart_a_task_that_exited_cleanly() {
        let state = SupervisedTask::new();
        let runs = Arc::new(AtomicUsize::new(0));

        let runs_for_task = runs.clone();
        let supervisor = tokio::spawn(supervise(
            state.clone(),
            "test",
            fast_config(5),
            move || {
                let runs = runs_for_task.clone();
                async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                }
            },
        ));

        supervisor.await.expect("supervisor joins");
        assert_eq!(state.restart_count(), 0);
        assert!(!state.is_alive());
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    /// The panel must never claim a dead writer is alive: when the
    /// supervisor goes away, the flag has to follow it.
    #[tokio::test]
    async fn the_supervisor_clears_the_alive_flag_when_it_is_aborted() {
        let state = SupervisedTask::new();

        let supervisor = tokio::spawn(supervise(
            state.clone(),
            "test",
            fast_config(5),
            move || async {
                // A writer that never ends: only the abort stops it.
                std::future::pending::<()>().await;
            },
        ));

        assert!(
            wait_for(|| state.is_alive()).await,
            "the writer task must be running"
        );
        supervisor.abort();
        let _ = supervisor.await;
        assert!(
            !state.is_alive(),
            "an aborted supervisor must not leave the writer looking alive"
        );
    }
}
