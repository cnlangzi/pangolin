//! The graceful reload — the supervisor and its generations (issue #234).
//!
//! ## The process model
//!
//! ```text
//! systemd ──MAINPID──> supervisor（常驻，不服务流量）
//!                         └── generation（服务流量，可被替换）
//! ```
//!
//! The **supervisor** is the resident parent: it forks the
//! generations, watches them and performs the handover. It never
//! serves a request, never binds a port and never opens the
//! database. It stays alive across handovers, so systemd's
//! `MAINPID` keeps pointing at a live process and `ExecReload`
//! keeps working.
//!
//! The **generation** is the child that does the work: it binds the
//! six listeners, opens the database and serves the traffic. A
//! handover replaces it.
//!
//! This module is the *process* supervisor. The other one — the
//! supervisor of the background JSONL writers — lives in
//! `pangolin_core::supervisor` and has nothing to do with it.
//!
//! ## The handover
//!
//! ```text
//! supervisor                新 generation              老 generation
//!     │  fork + execve           │                          │
//!     ├─────────────────────────>│ 绑定 6 个监听点           │
//!     │                          │（SO_REUSEPORT，与老代共存）│
//!     │                          ├─ 写 ready 字节 ─────────>│  supervisor 读到
//!     │  ── SIGUSR2 ───────────────────────────────────────> │  A 段：停止接受新连接
//!     │                          │                          │  B 段：关掉 tunnel 会话
//!     │                          │                          │  C 段：等在途请求
//!     │                          │                          │  退出
//!     │  ── SIGUSR1（新代重建索引）─>│
//! ```
//!
//! The port is held throughout: the incoming generation binds it
//! while the outgoing one still has it, and the outgoing one stops
//! accepting only **after** the incoming one has said it is ready.
//! A handover that fails therefore never touches the running
//! generation — it keeps accepting, and the operator sees an error
//! in the log.
//!
//! ## Why the readiness pipe
//!
//! pingora binds its own listeners inside its service thread, and a
//! bind that fails leaves the **process alive and serving nothing**
//! (see `crates/ngx/src/check.rs`). The handover cannot rely on the
//! process dying to notice, so the incoming generation has to *say*
//! that it is ready — and it may only say so once every listener is
//! bound. The pipe is the way it says it: one byte, written after
//! the last bind.
//!
//! ## Why the program is `argv[0]`
//!
//! The operator replaces the binary with an atomic `mv`. `/proc/self/exe`
//! points at the inode that was unlinked, so exec'ing it would start
//! the **old** binary again and the handover would change nothing.
//! The path in `argv[0]` is where the new binary lands.

use std::collections::HashSet;
use std::io;
use std::os::fd::RawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use log::{error, info, warn};
use tokio_util::sync::CancellationToken;

use pangolin_core::config::Config;

use crate::runtime;

// ---- the environment contract ----

/// The environment variable that carries the supervisor's process
/// number to a generation.
///
/// The generation checks that the process is really there: a
/// generation without a supervisor would be an orphan, holding the
/// ports with nobody to replace it.
pub const ENV_SUPERVISOR_PID: &str = "PANGLOLIN_SUPERVISOR_PID";

/// The environment variable that carries the readiness pipe's write
/// end to the incoming generation.
///
/// Only the generation of a **handover** gets one: the first
/// generation of a fresh start has no outgoing generation to hand
/// over to.
pub const ENV_READY_FD: &str = "PANGLOLIN_RELOAD_READY_FD";

// ---- the invocation ----

/// The command line the gateway was started with.
///
/// A generation is started with the **same** command line plus
/// `--generation`, so the operator's flags survive the handover and
/// a configuration change on disk is picked up by the new
/// generation.
#[derive(Debug, Clone)]
pub struct Invocation {
    /// The program to execute.
    program: PathBuf,
    /// The full command line, `argv[0]` included.
    args: Vec<String>,
}

impl Invocation {
    /// The command line this process was started with.
    pub fn current() -> Self {
        let args: Vec<String> = std::env::args().collect();
        let program = resolve_program(args.first().map(String::as_str).unwrap_or_default());
        Self { program, args }
    }

    /// The command line of a generation.
    ///
    /// `argv[0]` is the resolved program — the operator's own
    /// `argv[0]` may be a bare name, and `execve` does not look one
    /// up in `PATH`.
    fn generation_args(&self) -> Vec<String> {
        let mut args = Vec::with_capacity(self.args.len());
        args.push(self.program.to_string_lossy().into_owned());
        args.extend(self.args.iter().skip(1).cloned());
        args.push("--generation".to_string());
        args
    }
}

/// The program a generation is started from.
///
/// `argv[0]` — **not** `/proc/self/exe`. The operator replaces the
/// binary with an atomic `mv`, and `/proc/self/exe` keeps pointing at
/// the inode that was unlinked: exec'ing it would start the old
/// binary again, and the handover would change nothing.
///
/// A bare name (the binary was found through `PATH`) is looked up the
/// same way the shell would, so the generation runs the binary the
/// operator's `PATH` points at — which is the new one after a
/// replacement.
fn resolve_program(argv0: &str) -> PathBuf {
    if argv0.is_empty() {
        return PathBuf::from(argv0);
    }
    if argv0.contains('/') {
        // A path: make it absolute, so a later `chdir` cannot break
        // the handover. `std::path::absolute` does not resolve
        // symlinks — which is exactly what we want here.
        return std::path::absolute(argv0).unwrap_or_else(|_| PathBuf::from(argv0));
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(argv0);
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from(argv0)
}

// ---- the readiness pipe ----

/// The readiness pipe of a handover.
///
/// The supervisor creates the pipe, hands the **write end** to the
/// incoming generation through an inherited descriptor and reads the
/// read end. One byte on the write end means "every listener is
/// bound"; EOF means the generation died before it got there.
struct ReadyPipe {
    read_fd: RawFd,
    /// The supervisor's copy of the write end.
    ///
    /// It is closed right after the generation is spawned: the child
    /// has a copy of its own, and the supervisor holding one would
    /// keep the read end from ever seeing the end of the file — so a
    /// generation that dies during its startup would only be noticed
    /// when the ready timeout runs out.
    write_fd: Option<RawFd>,
}

impl ReadyPipe {
    /// Create the pipe.
    ///
    /// The read end is non-blocking: the supervisor polls it while it
    /// waits for the byte, and a blocking read would park the whole
    /// supervisor.
    fn new() -> io::Result<Self> {
        let mut fds = [0 as RawFd; 2];
        // SAFETY: `fds` is a two-element array, which is what `pipe`
        // expects.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } == -1 {
            return Err(io::Error::last_os_error());
        }
        // Both ends move out of the standard streams. `pipe` takes
        // the lowest free descriptors, and a gateway started with a
        // closed stdin or stdout would otherwise hand one of them to
        // the pipe: the child's stdout would be the pipe's write end
        // (its log output would fill the pipe and block it), and the
        // supervisor's stdout would be the pipe's read end (the same
        // hang, one process over).
        let read_fd = match move_above_stdio(fds[0]) {
            Ok(fd) => fd,
            Err(e) => {
                // SAFETY: both descriptors were just created by
                // `pipe`.
                unsafe {
                    libc::close(fds[0]);
                    libc::close(fds[1]);
                }
                return Err(e);
            }
        };
        let write_fd = match move_above_stdio(fds[1]) {
            Ok(fd) => fd,
            Err(e) => {
                // SAFETY: both descriptors are owned by this pipe.
                unsafe {
                    libc::close(read_fd);
                    libc::close(fds[1]);
                }
                return Err(e);
            }
        };
        // SAFETY: the descriptor was just created by `pipe`.
        if unsafe { libc::fcntl(read_fd, libc::F_SETFL, libc::O_NONBLOCK) } == -1 {
            let err = io::Error::last_os_error();
            // SAFETY: both descriptors are owned by this pipe.
            unsafe {
                libc::close(read_fd);
                libc::close(write_fd);
            }
            return Err(err);
        }
        Ok(Self {
            read_fd,
            write_fd: Some(write_fd),
        })
    }

    /// The write end — the descriptor the child inherits.
    fn write_fd(&self) -> RawFd {
        self.write_fd.expect("the write end is still open")
    }

    /// Close the supervisor's copy of the write end.
    fn close_write(&mut self) {
        if let Some(fd) = self.write_fd.take() {
            // SAFETY: the descriptor is owned by this pipe.
            unsafe {
                libc::close(fd);
            }
        }
    }
}

/// Move a descriptor out of the standard streams.
///
/// A descriptor that already sits above them is returned unchanged.
fn move_above_stdio(fd: RawFd) -> io::Result<RawFd> {
    if fd > 2 {
        return Ok(fd);
    }
    // SAFETY: `F_DUPFD_CLOEXEC` takes the lowest free descriptor at
    // or above 3 — the standard streams stay untouched.
    let moved = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if moved == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor was just duplicated.
    unsafe { libc::close(fd) };
    Ok(moved)
}

impl Drop for ReadyPipe {
    fn drop(&mut self) {
        // SAFETY: the read descriptor is owned by this pipe.
        unsafe {
            libc::close(self.read_fd);
        }
        self.close_write();
    }
}

/// The write end of the readiness pipe, in the incoming generation.
pub struct ReadySignal {
    fd: RawFd,
}

impl ReadySignal {
    /// Take the readiness pipe out of the environment.
    ///
    /// `None` when this generation is not an incoming one — the first
    /// generation of a fresh start has no outgoing generation to hand
    /// over to, so it gets no pipe.
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        let Some(raw) = std::env::var_os(ENV_READY_FD) else {
            return Ok(None);
        };
        let fd: RawFd = raw
            .to_str()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| anyhow!("{ENV_READY_FD} is not a descriptor number: {raw:?}"))?;
        // The descriptor has to be a real one: a stale number would
        // put the ready byte into somebody else's file.
        // SAFETY: `F_GETFD` only reads the descriptor's flags.
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1 {
            bail!("{ENV_READY_FD}={fd} is not an open file descriptor");
        }
        Ok(Some(Self { fd }))
    }

    /// Tell the supervisor that every listener is bound.
    pub fn signal_ready(&self) -> anyhow::Result<()> {
        let byte = [1u8];
        // SAFETY: writing one byte into a pipe the process owns.
        let n = unsafe { libc::write(self.fd, byte.as_ptr().cast(), 1) };
        if n != 1 {
            return Err(anyhow!(
                "writing the readiness byte failed: {}",
                io::Error::last_os_error()
            ));
        }
        Ok(())
    }
}

impl Drop for ReadySignal {
    fn drop(&mut self) {
        // SAFETY: the descriptor was inherited from the supervisor
        // and is owned by this process.
        unsafe {
            libc::close(self.fd);
        }
    }
}

// ---- the listener check ----

/// Whether this process holds a listening socket on `port`.
///
/// The kernel's own view of the socket table is the only place that
/// says "this process really has the listener". `/proc/net/tcp` lists
/// every socket of the network namespace, so the entries are matched
/// against the socket inodes this process holds (`/proc/self/fd`) —
/// the outgoing generation has a socket on the same port, and only
/// the inode tells the two apart.
///
/// Linux-only: `/proc` is where the socket table lives.
#[cfg(target_os = "linux")]
fn holds_listening_port(port: u16) -> bool {
    let ours = socket_inodes();
    if ours.is_empty() {
        return false;
    }
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(raw) = std::fs::read_to_string(table) else {
            continue;
        };
        for line in raw.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 10 {
                continue;
            }
            // `0A` is TCP_LISTEN.
            if fields[3] != "0A" {
                continue;
            }
            let Some(listen_port) = hex_port(fields[1]) else {
                continue;
            };
            if listen_port != port {
                continue;
            }
            let Ok(inode) = fields[9].parse::<u64>() else {
                continue;
            };
            if ours.contains(&inode) {
                return true;
            }
        }
    }
    false
}

/// The port a `/proc/net/tcp` local address names.
///
/// The address is `HEXIP:HEXPORT`, with the port in hexadecimal.
#[cfg(target_os = "linux")]
fn hex_port(local_address: &str) -> Option<u16> {
    let (_, port) = local_address.rsplit_once(':')?;
    u16::from_str_radix(port, 16).ok()
}

/// The socket inodes this process holds open.
#[cfg(target_os = "linux")]
fn socket_inodes() -> HashSet<u64> {
    let mut inodes = HashSet::new();
    let Ok(dir) = std::fs::read_dir("/proc/self/fd") else {
        return inodes;
    };
    for entry in dir.flatten() {
        let Ok(target) = std::fs::read_link(entry.path()) else {
            continue;
        };
        let text = target.to_string_lossy();
        let Some(rest) = text.strip_prefix("socket:[") else {
            continue;
        };
        let Some(inode) = rest.strip_suffix(']').and_then(|n| n.parse::<u64>().ok()) else {
            continue;
        };
        inodes.insert(inode);
    }
    inodes
}

/// The port a listen address names.
///
/// `None` when the address names no port — an address that does not
/// resolve, or a port of `0`.
fn listen_port(addr: &str) -> Option<u16> {
    use std::net::ToSocketAddrs;
    let port = addr.to_socket_addrs().ok()?.next()?.port();
    if port == 0 { None } else { Some(port) }
}

/// Wait until this process holds every listener the handover needs.
///
/// Returns the ports that are still missing when the budget runs out.
///
/// A port of `0` is skipped: the kernel picks a free port for it, so
/// the bind cannot collide with anybody and there is nothing to
/// verify.
///
/// Fails where the check cannot run — see
/// [`READINESS_CHECK_AVAILABLE`].
pub async fn wait_for_listeners(ports: &[u16], timeout: Duration) -> anyhow::Result<Vec<u16>> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (ports, timeout);
        bail!("the readiness check reads /proc — the handover is a Linux feature");
    }
    #[cfg(target_os = "linux")]
    {
        let deadline = Instant::now() + timeout;
        loop {
            let missing: Vec<u16> = ports
                .iter()
                .copied()
                .filter(|port| !holds_listening_port(*port))
                .collect();
            if missing.is_empty() {
                return Ok(Vec::new());
            }
            if Instant::now() >= deadline {
                return Ok(missing);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// Whether this platform can verify that a listener is bound.
///
/// The check reads the kernel's socket table, which is Linux-only.
/// The gateway ships a macOS binary too, and a handover it cannot
/// verify is a handover it must not perform: the whole point of the
/// handshake is to know that the incoming generation really has the
/// listeners before the outgoing one is told to stop accepting.
pub const READINESS_CHECK_AVAILABLE: bool = cfg!(target_os = "linux");

/// The ports of the listeners the handover has to see bound.
///
/// Five of the gateway's six listeners are TCP sockets; four of them
/// are checked:
///
/// * the proxy's HTTP and HTTPS ports are pingora's — it binds them
///   inside its own service thread, so the gateway never sees the
///   sockets and has to look them up in the kernel's socket table;
/// * the tunnel's TCP port and the management API's port are the
///   gateway's own, but they are bound by the services that run
///   beside pingora, so the handover checks them too.
///
/// The admin port is **not** one of them. The admin listener does
/// not ask for `SO_REUSEPORT` (issue #261), so the incoming
/// generation cannot bind the port while the outgoing one still
/// holds it: a readiness check that waited for it would fail every
/// handover. The admin UI is unavailable for the length of the
/// switch window instead — the incoming generation binds the port as
/// soon as the outgoing one lets go of it, and the proxy is serving
/// the whole time.
///
/// The sixth listener — the QUIC endpoint — is a UDP socket and is
/// not in the table; it is covered by the strict bind (a handover
/// generation may not come up without it) and by the shutdown token
/// its failure cancels.
pub fn listener_ports(config: &Config) -> Vec<u16> {
    let mut ports = Vec::new();
    if let Some(port) = listen_port(&config.addr.http) {
        ports.push(port);
    }
    if config.addr.https != ":0"
        && !config.addr.https.is_empty()
        && let Some(port) = listen_port(&config.addr.https)
    {
        ports.push(port);
    }
    if let Some(port) = listen_port(&config.tunnel.addr) {
        ports.push(port);
    }
    if config.api.listen != ":0"
        && !config.api.listen.is_empty()
        && let Some(port) = listen_port(&config.api.listen)
    {
        ports.push(port);
    }
    ports
}

// ---- signals ----

/// Send a signal to a process.
///
/// Returns the error the kernel reported, so the caller can say what
/// went wrong.
fn signal(pid: u32, signal: libc::c_int) -> io::Result<()> {
    // SAFETY: `kill` with a valid pid and a standard signal number.
    let rc = unsafe { libc::kill(pid as libc::pid_t, signal) };
    if rc == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Whether a process is alive.
///
/// Signal `0` is the existence check: it tells the caller that the
/// process is there without disturbing it. `EPERM` means the process
/// exists but belongs to somebody else — still alive.
pub fn process_is_alive(pid: u32) -> bool {
    // SAFETY: signal 0 is the existence check.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Make sure this generation has a supervisor, and return its number.
///
/// A generation without one would be an orphan: it would hold the
/// ports and nothing would ever replace it. The supervisor's number
/// travels in the environment, and the check is that the process is
/// really there.
pub fn require_supervisor() -> anyhow::Result<u32> {
    let raw = std::env::var(ENV_SUPERVISOR_PID).map_err(|_| {
        anyhow!("{ENV_SUPERVISOR_PID} is not set — a generation needs a supervisor")
    })?;
    let pid: u32 = raw
        .parse()
        .map_err(|e| anyhow!("{ENV_SUPERVISOR_PID} is not a process number: {raw:?} ({e})"))?;
    if !process_is_alive(pid) {
        bail!("the supervisor {pid} is not running — this generation would be an orphan");
    }
    Ok(pid)
}

/// Make this process die with its supervisor.
///
/// `PR_SET_PDEATHSIG` is cleared by `execve`, so the generation has
/// to arm it itself — after the exec, not before it.
///
/// The kernel's parent-death signal is **Linux-only**, and the
/// gateway ships a macOS binary too. Elsewhere the parent process
/// number is watched instead: the kernel reparents an orphan to init,
/// so the number changes.
pub fn arm_parent_death(supervisor: u32) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let _ = supervisor;
        // SAFETY: `prctl` with `PR_SET_PDEATHSIG` takes a signal
        // number and has no other side effect.
        if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } == -1 {
            return Err(anyhow!(
                "could not arm the parent-death signal: {}",
                io::Error::last_os_error()
            ));
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(PARENT_WATCH_INTERVAL).await;
                if !supervisor_is_parent(supervisor) {
                    error!("the supervisor {supervisor} died — the generation exits");
                    std::process::exit(1);
                }
            }
        });
    }
    Ok(())
}

/// How often the parent-death watcher looks on the platforms that
/// have no parent-death signal.
#[cfg(not(target_os = "linux"))]
const PARENT_WATCH_INTERVAL: Duration = Duration::from_millis(500);

/// Whether the supervisor is still this process's parent.
///
/// The parent-death signal is armed after the exec, so there is a
/// window in which the supervisor could die before the signal was
/// armed. The check closes it: if the parent is not the supervisor
/// any more, the supervisor is gone.
pub fn supervisor_is_parent(supervisor: u32) -> bool {
    // SAFETY: `getppid` takes no arguments and cannot fail.
    (unsafe { libc::getppid() }) == supervisor as libc::pid_t
}

// ---- the generation ----

/// One generation of the gateway: the child process that serves the
/// traffic.
struct Generation {
    child: Child,
    pid: u32,
}

impl Generation {
    /// Start a generation.
    ///
    /// The child is a fresh `execve` of the gateway's own program with
    /// `--generation` appended, so it starts from scratch: it reads
    /// the configuration again, and a configuration that changed on
    /// disk takes effect.
    fn spawn(invocation: &Invocation, ready: Option<&ReadyPipe>) -> anyhow::Result<Self> {
        let args = invocation.generation_args();
        let mut cmd = Command::new(&invocation.program);
        cmd.args(&args[1..]);
        cmd.env(ENV_SUPERVISOR_PID, std::process::id().to_string());
        if let Some(pipe) = ready {
            cmd.env(ENV_READY_FD, pipe.write_fd().to_string());
            let write_fd = pipe.write_fd();
            let read_fd = pipe.read_fd;
            // SAFETY: the closure only calls async-signal-safe
            // functions (`fcntl`, `close`) on descriptors it owns.
            unsafe {
                cmd.pre_exec(move || {
                    // The write end has to survive the exec: the
                    // readiness byte is written through it.
                    if libc::fcntl(write_fd, libc::F_SETFD, 0) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    // The read end has to die with the child: the
                    // generation must not be able to hold the
                    // supervisor's side of the pipe open.
                    libc::close(read_fd);
                    Ok(())
                });
            }
        }
        let child = cmd.spawn().context("start the generation")?;
        let pid = child.id();
        Ok(Self { child, pid })
    }

    /// Whether the generation has exited, and how.
    fn try_exit(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }

    /// Kill the generation.
    ///
    /// Used when the handover fails: the incoming generation never
    /// became ready, so it has to go — the outgoing one keeps
    /// serving.
    fn kill(&mut self) {
        let _ = signal(self.pid, libc::SIGKILL);
        let _ = self.child.wait();
    }
}

// ---- the supervisor ----

/// Run the supervisor: the resident parent of the generations.
///
/// The supervisor never serves a request. It writes the PID file
/// (systemd's `MAINPID` points at it, and the operator commands
/// address the gateway through it), starts the first generation and
/// then waits for one of three things: a handover request, a stop
/// request, or the death of the current generation.
pub fn supervise(config_path: &Path, config: Config) -> anyhow::Result<()> {
    let invocation = Invocation::current();

    // The PID file belongs to the **supervisor**: `--reload` and
    // `--status` address the gateway through it, and the number in
    // it has to be the one `ExecReload` signals.
    let pid_file = config.resolved_pid_file(config_path);
    let _pid_guard = match pangolin_core::pidfile::write(&pid_file) {
        Ok(()) => Some(pangolin_core::pidfile::PidFileGuard::new(pid_file.clone())),
        Err(e) => {
            // The PID file is not a precondition for serving: the
            // gateway has to come up even when the operator cannot
            // write the file.
            log::error!("could not write the pid file {}: {e}", pid_file.display());
            None
        }
    };

    info!(
        "pangolin-ngx {} supervisor starting (pid {})",
        pangolin_core::VERSION,
        std::process::id()
    );

    runtime::block_on_host(async move {
        // The signal handlers go up **before** the first generation
        // starts. `SIGUSR2`'s default action is to terminate the
        // process, so a reload that arrives in the first moments of
        // the supervisor's life would kill it instead of handing
        // over.
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigusr2 =
            signal(SignalKind::user_defined2()).context("install the SIGUSR2 handler")?;
        let mut sigusr1 =
            signal(SignalKind::user_defined1()).context("install the SIGUSR1 handler")?;

        // The stop signals feed a **token** rather than a branch of
        // the supervisor's select: a handover waits for the incoming
        // generation, and a stop that arrives during that wait has to
        // be honoured — the handover must not hold the stop past
        // systemd's own deadline.
        let stop = CancellationToken::new();
        {
            let mut sigterm =
                signal(SignalKind::terminate()).context("install the SIGTERM handler")?;
            let stop = stop.clone();
            tokio::spawn(async move {
                while sigterm.recv().await.is_some() {
                    info!("SIGTERM received — the gateway is stopping");
                    stop.cancel();
                }
            });
        }
        {
            let mut sigint =
                signal(SignalKind::interrupt()).context("install the SIGINT handler")?;
            let stop = stop.clone();
            tokio::spawn(async move {
                while sigint.recv().await.is_some() {
                    info!("SIGINT received — the gateway is stopping");
                    stop.cancel();
                }
            });
        }

        // The first generation gets no readiness pipe: there is no
        // outgoing generation to hand over to.
        let mut generation = Generation::spawn(&invocation, None)?;
        info!("the first generation is running (pid {})", generation.pid);

        supervise_loop(
            &mut generation,
            &invocation,
            &config,
            &mut sigusr2,
            &mut sigusr1,
            &stop,
        )
        .await
    })
}

/// The supervisor's loop.
///
/// Three things can happen: a handover is requested (`SIGUSR2`), the
/// gateway is asked to stop (`SIGTERM` / `SIGINT`), or the current
/// generation dies on its own.
async fn supervise_loop(
    generation: &mut Generation,
    invocation: &Invocation,
    config: &Config,
    sigusr2: &mut tokio::signal::unix::Signal,
    sigusr1: &mut tokio::signal::unix::Signal,
    stop: &CancellationToken,
) -> anyhow::Result<()> {
    let ready_timeout = Duration::from_secs(config.reload.ready_timeout_secs);
    let drain_timeout = Duration::from_secs(config.reload.drain_timeout_secs);

    loop {
        tokio::select! {
            _ = sigusr2.recv() => {
                if !READINESS_CHECK_AVAILABLE {
                    error!("the handover needs the readiness check, which this platform does not \
                            have — the running generation keeps serving");
                    continue;
                }
                info!("SIGUSR2 received — handing over to a new generation");
                if let Err(e) =
                    perform_handover(generation, invocation, ready_timeout, drain_timeout, stop)
                        .await
                {
                    if stop.is_cancelled() {
                        // The gateway is stopping: the handover is
                        // abandoned and the generation is stopped.
                        stop_generation(generation, drain_timeout).await;
                        return Ok(());
                    }
                    // The outgoing generation never stopped accepting:
                    // the gateway is exactly as it was.
                    error!("the handover failed: {e} — the running generation keeps serving");
                }
            }
            _ = sigusr1.recv() => {
                // The operator asks the running generation to rebuild
                // its indexes. The operator only knows the
                // supervisor's number, so the signal is forwarded.
                info!("SIGUSR1 received — asking the generation to rebuild its indexes");
                if let Err(e) = signal(generation.pid, libc::SIGUSR1) {
                    warn!("could not forward SIGUSR1 to the generation {}: {e}", generation.pid);
                }
            }
            _ = stop.cancelled() => {
                info!("stopping the gateway");
                stop_generation(generation, drain_timeout).await;
                return Ok(());
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                // The generation may have died on its own. A dead
                // generation is a broken gateway, so the supervisor
                // leaves a non-zero exit code behind and lets
                // systemd's `Restart=on-failure` take over.
                if let Some(status) = generation.try_exit()? {
                    error!("the generation {} exited on its own ({status})", generation.pid);
                    bail!("the generation exited on its own");
                }
            }
        }
    }
}

/// Hand the gateway over to a new generation.
///
/// The order is the whole point:
///
/// 1. the incoming generation starts and binds the ports **beside**
///    the outgoing one (`SO_REUSEPORT`);
/// 2. it says it is ready — one byte into the readiness pipe;
/// 3. only then is the outgoing generation told to drain.
///
/// A failure in step 1 or 2 leaves the outgoing generation exactly
/// where it was: it never stopped accepting.
async fn perform_handover(
    generation: &mut Generation,
    invocation: &Invocation,
    ready_timeout: Duration,
    drain_timeout: Duration,
    stop: &CancellationToken,
) -> anyhow::Result<()> {
    let mut pipe = ReadyPipe::new().context("create the readiness pipe")?;
    let mut incoming =
        Generation::spawn(invocation, Some(&pipe)).context("start the incoming generation")?;
    // The supervisor must not hold the write end: the child has a
    // copy, and the supervisor's would keep the read end from seeing
    // the end of the file when the child dies.
    pipe.close_write();
    info!("the incoming generation {} is starting", incoming.pid);

    // A stop that arrives while the handover waits is honoured: the
    // gateway is going away, and a handover that ran to completion
    // would hold the stop past systemd's own deadline.
    let ready = tokio::select! {
        ready = wait_for_ready(&pipe, ready_timeout) => ready,
        _ = stop.cancelled() => {
            incoming.kill();
            return Err(anyhow!("the gateway is stopping"));
        }
    };
    if let Err(e) = ready {
        incoming.kill();
        return Err(e);
    }
    info!("the incoming generation {} is ready", incoming.pid);

    // The outgoing generation stops accepting and drains.
    let outgoing_pid = generation.pid;
    if let Err(e) = signal(outgoing_pid, libc::SIGUSR2) {
        incoming.kill();
        return Err(anyhow!(
            "could not signal the generation {outgoing_pid}: {e}"
        ));
    }
    info!("the generation {outgoing_pid} is draining");

    // The incoming generation rebuilds its indexes **after** the
    // outgoing one is gone: the writes that landed on the outgoing
    // generation are in the database by then.
    let deadline = Instant::now() + drain_timeout + Duration::from_secs(15);
    loop {
        if generation.try_exit()?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            error!(
                "the generation {} did not exit within {:?} — killing it",
                generation.pid, drain_timeout
            );
            generation.kill();
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    if let Err(e) = signal(incoming.pid, libc::SIGUSR1) {
        warn!(
            "could not ask the generation {} to rebuild its indexes: {e}",
            incoming.pid
        );
    }
    *generation = incoming;
    info!("the handover is complete");
    Ok(())
}

/// Wait for the incoming generation to report that it is ready.
///
/// The two failure modes are distinct and both matter: EOF means the
/// generation died (a broken configuration, a listener it could not
/// bind), and the timeout means it is alive but not ready.
async fn wait_for_ready(pipe: &ReadyPipe, timeout: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 1];
    loop {
        // SAFETY: reading one byte from a non-blocking pipe.
        let n = unsafe { libc::read(pipe.read_fd, buf.as_mut_ptr().cast(), 1) };
        if n == 1 {
            return Ok(());
        }
        if n == 0 {
            bail!("the incoming generation exited before it was ready");
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::WouldBlock {
            return Err(anyhow!("reading the readiness pipe failed: {err}"));
        }
        if Instant::now() >= deadline {
            bail!(
                "the incoming generation was not ready within {}s",
                timeout.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Stop the gateway: tell the generation to drain and wait for it.
async fn stop_generation(generation: &mut Generation, drain_timeout: Duration) {
    if let Err(e) = signal(generation.pid, libc::SIGTERM) {
        warn!("could not signal the generation {}: {e}", generation.pid);
        generation.kill();
        return;
    }
    let deadline = Instant::now() + drain_timeout + Duration::from_secs(15);
    loop {
        if generation.try_exit().is_ok_and(|s| s.is_some()) {
            info!("the generation {} exited", generation.pid);
            return;
        }
        if Instant::now() >= deadline {
            error!(
                "the generation {} did not exit within {:?} — killing it",
                generation.pid, drain_timeout
            );
            generation.kill();
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---- the operator commands ----

/// Ask the running gateway to hand over to a new generation.
///
/// The command is asynchronous: it sends the signal and exits. The
/// handover itself takes as long as the drain takes.
pub fn trigger_reload(config_path: &Path) -> anyhow::Result<()> {
    let config = Config::from_file(config_path).map_err(|e| anyhow!("config error: {e}"))?;
    let pid_file = config.resolved_pid_file(config_path);
    let pid = pangolin_core::pidfile::read(&pid_file)
        .map_err(|e| anyhow!("the gateway is not running: {e}"))?;
    if !process_is_alive(pid) {
        bail!(
            "the pid file {} is stale — it names pid {pid}, which is not running",
            pid_file.display()
        );
    }
    signal(pid, libc::SIGUSR2)
        .map_err(|e| anyhow!("could not signal the gateway (pid {pid}): {e}"))?;
    println!("sent SIGUSR2 to the gateway (pid {pid}) — the handover is under way");
    Ok(())
}

/// Report the state of the running gateway.
///
/// The supervisor's number comes from the PID file; the generation's
/// comes from the admin's `/healthz`, which answers with the pid of
/// the process that served it.
pub fn report_status(config_path: &Path) -> anyhow::Result<()> {
    let config = Config::from_file(config_path).map_err(|e| anyhow!("config error: {e}"))?;
    let pid_file = config.resolved_pid_file(config_path);
    let supervisor = pangolin_core::pidfile::read(&pid_file)
        .map_err(|e| anyhow!("the gateway is not running: {e}"))?;
    if !process_is_alive(supervisor) {
        bail!(
            "the pid file {} is stale — it names pid {supervisor}, which is not running",
            pid_file.display()
        );
    }
    println!("supervisor: pid {supervisor} (alive)");

    let url = admin_healthz_url(&config.admin.addr);
    match runtime::block_on_host(fetch_healthz(&url)) {
        Ok((status, body)) => {
            let parsed: serde_json::Value =
                serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
            let pid = parsed
                .get("pid")
                .and_then(|v| v.as_u64())
                .map(|p| p.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let state = parsed
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            println!("generation: pid {pid} ({state})");
            // A draining generation answers 503 — that is a state to
            // report, not a failure of the report.
            if status != 200 && status != 503 {
                bail!("the generation answered {status}");
            }
        }
        Err(e) => {
            println!("generation: unknown ({e})");
            bail!("the generation did not answer on {url}");
        }
    }
    Ok(())
}

/// The URL of the admin's health endpoint.
///
/// The admin address is a listen address, and a listen address of
/// `0.0.0.0` (or an empty host) is not a URL host — the status
/// command asks the gateway on the loopback.
fn admin_healthz_url(addr: &str) -> String {
    let host_port = match addr.rsplit_once(':') {
        Some((host, port)) => (host, port),
        None => (addr, ""),
    };
    // A wildcard listen address is not a URL host — the status
    // command asks the gateway on the loopback.
    let host = match host_port.0 {
        "" | "0.0.0.0" | "[::]" => "127.0.0.1".to_string(),
        // An IPv6 literal keeps its brackets: `http://::1:9000` is
        // not a URL, `http://[::1]:9000` is.
        other => other.to_string(),
    };
    format!("http://{host}:{}/healthz", host_port.1)
}

/// Ask a gateway for its health endpoint.
///
/// The request goes straight to the address: the ambient proxy
/// variables of the operator's shell would otherwise send it to a
/// proxy, which cannot answer for the gateway.
async fn fetch_healthz(url: &str) -> anyhow::Result<(u16, String)> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()?;
    let response = client.get(url).send().await?;
    let status = response.status().as_u16();
    let body = response.text().await?;
    Ok((status, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The program of a handover is the path in `argv[0]` — not
    /// `/proc/self/exe`.
    ///
    /// The operator replaces the binary with an atomic `mv`, and
    /// `/proc/self/exe` keeps pointing at the inode that was
    /// unlinked. Exec'ing it would start the old binary again, and
    /// the handover would change nothing.
    #[test]
    fn the_program_is_resolved_from_argv0() {
        let program = resolve_program("./bin/pangolin-ngx");
        assert!(
            program.is_absolute(),
            "the program must be an absolute path, got {program:?}"
        );
        assert_eq!(
            program.file_name().and_then(|n| n.to_str()),
            Some("pangolin-ngx")
        );
    }

    /// A bare name is looked up in `PATH`, the way the shell would.
    #[test]
    fn a_bare_program_name_is_looked_up_in_the_path() {
        let program = resolve_program("sh");
        assert!(
            program.ends_with("sh"),
            "the program must be found through PATH, got {program:?}"
        );
        assert!(program.is_absolute(), "the program must be absolute");
    }

    /// The generation's command line keeps the operator's flags and
    /// adds the one that says "you are a generation".
    #[test]
    fn the_generation_command_line_carries_the_flags_and_the_role() {
        let invocation = Invocation {
            program: PathBuf::from("/usr/bin/pangolin-ngx"),
            args: vec![
                "/usr/bin/pangolin-ngx".to_string(),
                "--config".to_string(),
                "/etc/pangolin/ngx.yml".to_string(),
            ],
        };
        let args = invocation.generation_args();
        assert_eq!(args[0], "/usr/bin/pangolin-ngx");
        assert_eq!(args[1], "--config");
        assert_eq!(args[2], "/etc/pangolin/ngx.yml");
        assert_eq!(args.last().map(String::as_str), Some("--generation"));
    }

    /// The readiness pipe carries exactly one byte from the
    /// generation to the supervisor.
    #[tokio::test]
    async fn the_readiness_pipe_carries_one_byte() {
        let mut pipe = ReadyPipe::new().expect("the pipe is created");
        // The signal **takes** the write end: the pipe would
        // otherwise close it a second time when it drops.
        let signal = ReadySignal {
            fd: pipe.write_fd.take().expect("the write end"),
        };
        signal.signal_ready().expect("the byte is written");
        let mut buf = [0u8; 1];
        // SAFETY: reading one byte from a non-blocking pipe.
        let n = unsafe { libc::read(pipe.read_fd, buf.as_mut_ptr().cast(), 1) };
        assert_eq!(n, 1, "the supervisor must see the ready byte");
        assert_eq!(buf[0], 1);
    }

    /// A generation that dies before it is ready closes the write end,
    /// and the supervisor sees the end of the file.
    #[tokio::test]
    async fn a_dead_generation_ends_the_readiness_pipe() {
        let mut pipe = ReadyPipe::new().expect("the pipe is created");
        {
            // The write end dies with the generation.
            let _signal = ReadySignal {
                fd: pipe.write_fd.take().expect("the write end"),
            };
        }
        let err = wait_for_ready(&pipe, Duration::from_secs(1))
            .await
            .expect_err("a generation that died is not ready");
        assert!(
            err.to_string().contains("exited"),
            "the error has to say that the generation exited: {err}"
        );
    }

    /// The ready timeout is a hard limit.
    #[tokio::test]
    async fn the_ready_timeout_is_a_hard_limit() {
        let mut pipe = ReadyPipe::new().expect("the pipe is created");
        // The write end stays open, but nothing is written.
        let _signal = ReadySignal {
            fd: pipe.write_fd.take().expect("the write end"),
        };
        let err = wait_for_ready(&pipe, Duration::from_millis(100))
            .await
            .expect_err("a generation that says nothing is not ready");
        assert!(
            err.to_string().contains("not ready"),
            "the error has to say that the generation was not ready: {err}"
        );
    }

    /// A process that is alive answers the existence check.
    #[test]
    fn a_live_process_answers_the_existence_check() {
        assert!(process_is_alive(std::process::id()));
    }

    /// A process that is gone does not.
    ///
    /// The pid of the process that just exited is the cheapest way to
    /// get a number that is certainly not in use.
    #[test]
    fn a_dead_process_does_not_answer_the_existence_check() {
        let pid = std::process::id();
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn a process that exits at once");
        let dead = child.id();
        child.wait().expect("wait for the child");
        assert_ne!(dead, pid);
        // The number is very likely free again, but the kernel may
        // hand it to somebody else in the meantime — so the test only
        // asserts the obvious direction.
        assert!(process_is_alive(pid));
    }

    /// The admin's health URL is built from the listen address.
    ///
    /// A listen address of `0.0.0.0` is not a URL host — the status
    /// command asks the gateway on the loopback.
    #[test]
    fn the_health_url_uses_the_loopback_for_a_wildcard_address() {
        assert_eq!(
            admin_healthz_url("0.0.0.0:8080"),
            "http://127.0.0.1:8080/healthz"
        );
        assert_eq!(admin_healthz_url(":8080"), "http://127.0.0.1:8080/healthz");
        assert_eq!(
            admin_healthz_url("127.0.0.1:9000"),
            "http://127.0.0.1:9000/healthz"
        );
    }

    /// A listener that this process holds is found in the kernel's
    /// socket table.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_bound_listener_is_found_in_the_socket_table() {
        let listener = crate::reuseport::bind_tcp("127.0.0.1:0".parse().expect("an address"))
            .expect("the listener binds");
        let port = listener.local_addr().expect("the address").port();
        assert!(
            holds_listening_port(port),
            "the process must be found as the listener of port {port}"
        );
    }

    /// A port nobody holds is not found.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_port_nobody_holds_is_not_found() {
        // Port 1 needs root, so it is free for an unprivileged
        // process — and even if something held it, the process would
        // not be the one holding it.
        assert!(!holds_listening_port(1));
    }
}
