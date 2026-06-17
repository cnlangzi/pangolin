//! Real-binary e2e test harness.
//!
//! Each e2e test that needs a running `pangolin-ngx` (and optionally a
//! `pangolin-tun`) gets fresh subprocesses via the wrappers in this
//! module. Unlike the rest of the test suite — which exercises
//! `pangolin-core` as a library with in-process mock backends — these
//! tests drive the **actual binaries** the way production would:
//!   - real port binding
//!   - real signal handling
//!   - real TLS handshake
//!   - real WebSocket upgrade
//!   - real CLI arg parsing
//!
//! The wrappers allocate a free port, generate a per-test TOML, spawn
//! the binary, and poll for readiness before returning. On drop they
//! SIGTERM the child, give it 2s to exit, then SIGKILL.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

// Global port allocator to prevent race conditions in parallel tests
lazy_static::lazy_static! {
    static ref PORT_ALLOCATOR: Mutex<HashSet<u16>> = Mutex::new(HashSet::new());
}

/// Path to the workspace's `target/release/` directory. Both binaries
/// are expected to be there — the Makefile's `build` target produces
/// them, and the test runner depends on `make build` via `test-e2e`.
fn target_release() -> PathBuf {
    // CARGO_MANIFEST_DIR is the `tests/` crate; the workspace root is
    // its parent.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    Path::new(manifest_dir)
        .parent()
        .expect("workspace root")
        .join("target")
        .join("release")
}

fn ngx_binary() -> PathBuf {
    resolve_binary("pangolin-ngx", "ngx")
}

fn tun_binary() -> PathBuf {
    resolve_binary("pangolin-tun", "tun")
}

/// Find the binary, preferring the Makefile-managed `bin/` copies
/// (used after `make build`) and falling back to `target/release/`
/// (used when developers run `cargo build` directly). Returns the
/// first one that exists; if neither does the caller panics with a
/// helpful message.
fn resolve_binary(makefile_name: &str, cargo_name: &str) -> PathBuf {
    // target_release() = <workspace>/target/release
    // .parent()        = <workspace>/target
    // .parent().unwrap() = <workspace>
    let target = target_release();
    let workspace_root = target.parent().and_then(|p| p.parent()).unwrap();
    let bin = workspace_root.join("bin").join(makefile_name);
    if bin.exists() {
        return bin;
    }
    let direct = target.join(cargo_name);
    if direct.exists() {
        return direct;
    }
    // Return the Makefile path so the caller's panic message names
    // the canonical location.
    bin
}

/// Reserve a free TCP port with global coordination to prevent race conditions.
///
/// **Problem**: The naive approach (bind :0, get port, drop listener) has a
/// TOCTOU race window: between dropping the listener and the test actually
/// binding the port, another parallel test might grab the same port.
///
/// **Solution**: Use a global Mutex<HashSet<u16>> to track allocated ports.
/// Once a port is allocated, it stays reserved until explicitly released.
///
/// **Tradeoff**: There's still a small window between allocating and binding,
/// but it's much smaller than the naive approach. Tests clean up ports via
/// release_port() in Drop implementations.
pub fn free_port() -> u16 {
    let mut allocated = PORT_ALLOCATOR.lock().unwrap();

    // Try up to 100 times to find a free port
    for _ in 0..100 {
        // Bind to :0 to let the OS assign a free port
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0 for free port");
        let port = listener.local_addr().expect("local_addr").port();
        drop(listener); // Release immediately

        // Check if this port is already tracked as allocated
        if allocated.insert(port) {
            // Successfully reserved!
            return port;
        }
        // Port already allocated, try again
    }

    panic!("Failed to allocate a free port after 100 attempts");
}

/// Release a port back to the pool. Called automatically by NgxProcess::drop
/// and TunProcess::drop.
pub fn release_port(port: u16) {
    PORT_ALLOCATOR.lock().unwrap().remove(&port);
}

/// Issue a raw HTTP/1.1 request to `addr` with a caller-chosen
/// `Host` header, returning `(status, body)`. Bypasses reqwest
/// entirely — reqwest 0.12+ ignores user-supplied `Host` headers
/// (it always sets `Host` from the URL authority), which makes it
/// useless for testing virtual-host routing.
///
/// Both the connect and the read are guarded by a 5 s timeout so a
/// hung proxy (panic, deadlock, unreachable upstream) fails the
/// test instead of blocking the whole suite.
pub async fn raw_request(
    addr: &str,
    host: &str,
    method: &str,
    path: &str,
    body: &[u8],
) -> (u16, String) {
    raw_request_inner(
        addr, host, method, path, body, /* send_content_length */ true,
    )
    .await
}

/// Issue a raw HTTP/1.1 request **without** a `Content-Length` header.
///
/// `raw_request` always emits `Content-Length: 0`, which is enough to
/// keep the existing test suite green but **masks a real bug**: a
/// genuine curl GET ships zero body-framing headers, and the proxy
/// must still treat that as "no body". This helper exercises exactly
/// that path, mirroring what `curl -v http://…` puts on the wire.
pub async fn raw_request_no_content_length(
    addr: &str,
    host: &str,
    method: &str,
    path: &str,
) -> (u16, String) {
    raw_request_inner(
        addr, host, method, path, b"", /* send_content_length */ false,
    )
    .await
}

/// Shared low-level path for the two `raw_request*` helpers. The only
/// thing they vary on is whether the `Content-Length` framing header
/// is emitted; everything else (timeout, headers, response parsing)
/// is the same.
async fn raw_request_inner(
    addr: &str,
    host: &str,
    method: &str,
    path: &str,
    body: &[u8],
    send_content_length: bool,
) -> (u16, String) {
    let timeout = Duration::from_secs(5);

    let mut stream = tokio::time::timeout(timeout, TcpStream::connect(addr))
        .await
        .expect("connect to addr (5s timeout)")
        .expect("connect to addr");

    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nUser-Agent: pangolin-e2e\r\nAccept: */*\r\n",
    );
    if send_content_length {
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    stream
        .write_all(req.as_bytes())
        .await
        .expect("write request head");
    if !body.is_empty() {
        stream.write_all(body).await.expect("write body");
    }

    let mut buf = Vec::new();
    tokio::time::timeout(timeout, stream.read_to_end(&mut buf))
        .await
        .expect("read response (5s timeout)")
        .expect("read response");

    let text = String::from_utf8_lossy(&buf);
    let status = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

/// Async readiness check — keep trying `TcpStream::connect` until
/// either it succeeds or the timeout elapses.
///
/// `log` is the captured stdout+stderr of the child process. When the
/// timeout fires, the captured log is dumped into the panic message
/// so a flaky startup on CI is debuggable from the test failure alone
/// (no need to re-run with --nocapture and hope the timing reproduces).
async fn wait_for_port(port: u16, timeout: Duration, log: &Arc<Mutex<Vec<u8>>>) {
    let deadline = Instant::now() + timeout;
    loop {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return;
        }
        if Instant::now() >= deadline {
            let captured = {
                let buf = log.lock().unwrap();
                String::from_utf8_lossy(&buf).into_owned()
            };
            panic!(
                "pangolin-ngx did not start listening on port {} within {:?}.\n\
                 --- captured stdout+stderr ---\n{}\n--- end ---\n\
                 Common causes: cold-start of a release binary under CI load \
                 (the 30s budget here replaces the original 5s, which flaked \
                 in #56); port already in use (look for 'Address already in use' \
                 in the log above); config file parse error.",
                port, timeout, captured
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Generate a self-signed cert+key pair for the given SANs and write
/// them as a single autocert DirCache blob at `{cert_dir}/{host}`.
/// The blob format is: key PEM first, then cert chain PEM (matches
/// `acme.rs::build_blob` and Go's `autocert.DirCache` byte layout).
pub fn gen_self_signed(sans: &[&str], host: &str, cert_dir: &Path) -> PathBuf {
    let sans: Vec<String> = sans.iter().map(|s| s.to_string()).collect();
    let cert_key = rcgen::generate_simple_self_signed(sans).expect("generate self-signed cert");
    let cert_pem = cert_key.cert.pem();
    let key_pem = cert_key.signing_key.serialize_pem();

    // Autocert DirCache blob: key PEM first, then cert chain.
    let blob = format!("{}\n{}\n", key_pem.trim_end(), cert_pem.trim_end());
    let blob_path = cert_dir.join(host);
    std::fs::write(&blob_path, blob).expect("write autocert blob");
    blob_path
}

/// Create a fresh `pangolin.db` at `path` with the standard schema
/// applied via refinery migrations. The test then opens the same path
/// with rusqlite to insert sites/domains/tokens before the binary starts.
pub fn init_pangolin_db(path: &Path) {
    if path.exists() {
        std::fs::remove_file(path).expect("remove existing pangolin.db");
    }
    let mut conn = pangolin_core::db::open(path).expect("open pangolin.db");
    pangolin_core::db::migrate(&mut conn).expect("run migrations");
}

/// Shared, lock-protected log buffer (stdout+stderr) captured from
/// a running child process. The `Arc<Mutex<Vec<u8>>>` is shared
/// between the reader tasks and the test thread that wants to
/// dump it on failure.
type CapturedLog = Arc<Mutex<Vec<u8>>>;

/// Capture child stdout+stderr into a shared `Vec<u8>` so failing tests
/// can dump the binary's log output. Returns the `JoinHandle`s of the
/// reader tasks so `NgxProcess::drop` can `abort()` them — otherwise
/// they keep the tokio runtime alive after the test process tries to
/// exit, and `cargo test` hangs indefinitely.
fn spawn_with_log_capture(mut cmd: Command) -> (Child, CapturedLog, Vec<JoinHandle<()>>) {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn child process");
    let log = Arc::new(Mutex::new(Vec::<u8>::new()));
    let mut handles = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        let log = log.clone();
        handles.push(tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => log.lock().unwrap().extend_from_slice(&buf[..n]),
                    Err(_) => break,
                }
            }
        }));
    }
    if let Some(stderr) = child.stderr.take() {
        let log = log.clone();
        handles.push(tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => log.lock().unwrap().extend_from_slice(&buf[..n]),
                    Err(_) => break,
                }
            }
        }));
    }
    (child, log, handles)
}

/// RAII handle for a running `pangolin-ngx` subprocess. On drop, the
/// child is sent SIGTERM, polled for 2s, then SIGKILL'd if still alive.
pub struct NgxProcess {
    pub child: Option<Child>,
    pub http_port: u16,
    pub tls_port: u16,
    pub tunnel_port: u16,
    pub admin_port: u16,
    pub data_dir: PathBuf,
    pub config_path: PathBuf,
    pub cert_dir: PathBuf,
    pub log: Arc<Mutex<Vec<u8>>>,
    // tokio tasks draining the child's stdout/stderr into `log`. We
    // hold the JoinHandles so Drop can `abort()` them — otherwise they
    // outlive the test process and keep the tokio runtime (and thus
    // `cargo test`) alive forever.
    log_tasks: Vec<JoinHandle<()>>,
    // Hold the tempdir so it isn't dropped (and deleted) before the
    // child finishes using it.
    _tmpdir: TempDir,
}

impl NgxProcess {
    /// Spawn `pangolin-ngx` with a per-test config, wait for the HTTP
    /// port to accept connections, return a ready-to-use handle.
    ///
    /// `seed_db` is invoked with the path of the not-yet-created
    /// `pangolin.db` so the test can pre-populate sites/domains/tokens
    /// **before** the binary boots. The binary reads the DB and builds
    /// in-memory indexes on startup, so this is the only point at
    /// which seed data is visible without an explicit
    /// `reload_indexes` admin call.
    pub async fn start<F: FnOnce(&Path)>(seed_db: F) -> Self {
        let tmpdir = tempfile::Builder::new()
            .prefix("pangolin-e2e-ngx-")
            .tempdir()
            .expect("create tempdir for ngx e2e");
        let data_dir = tmpdir.path().join("data");
        std::fs::create_dir_all(&data_dir).expect("mkdir data");
        let cert_dir = tmpdir.path().join("certs");
        std::fs::create_dir_all(&cert_dir).expect("mkdir certs");
        // No `default` blob: v2 has no `default` SNI fallback
        // (see `pangolin_core::app::resolve_cert_fails_without_default_fallback`).
        // Generating one here would be auto-imported into the
        // `certs` table on every test boot, polluting cert counts
        // in unrelated tests.

        let http_port = free_port();
        let tls_port = free_port();
        let tunnel_port = free_port();
        let admin_port = free_port();

        let config = format!(
            r#"
addr:
  http: "127.0.0.1:{http}"
  https: "127.0.0.1:{tls}"
host: default

tunnel:
  addr: "127.0.0.1:{tunnel_port}"
  ws_path: /tunnel

log:
  level: info

admin:
  addr: 127.0.0.1:{admin}

acme:
  cert_dir: "{cert_dir}"
  email: ""
  # v2: no global autorenew — per-domain auto_issue lives in the DB
"#,
            http = http_port,
            tls = tls_port,
            tunnel_port = tunnel_port,
            admin = admin_port,
            cert_dir = cert_dir.display().to_string().replace('\\', "\\\\"),
        );
        let config_path = tmpdir.path().join("ngx.yml");
        std::fs::write(&config_path, config).expect("write ngx.yml");

        // The binary creates pangolin.db at runtime in CWD. We want
        // it in our tempdir so the test owns the DB lifecycle.
        let db_path = tmpdir.path().join("pangolin.db");
        seed_db(&db_path);

        // Sanity check: the binary must exist. The Makefile's `test-e2e`
        // depends on `build`, but if a developer runs `cargo test`
        // directly without building first, fail loudly with a helpful
        // message rather than a confusing "no such file".
        let bin = ngx_binary();
        if !bin.exists() {
            panic!(
                "pangolin-ngx binary not found at {}. Run `make build` (or `cargo build --release -p ngx -p tun`) first.",
                bin.display()
            );
        }

        let mut cmd = Command::new(&bin);
        cmd.arg("--config").arg(&config_path);
        // Spawn with CWD = tempdir so `pangolin.db` (which the binary
        // builds from the hardcoded relative path `pangolin.db`) lands
        // in the tempdir alongside the seeded data.
        cmd.current_dir(tmpdir.path());
        // env_logger in the binary reads RUST_LOG, not the
        // [log] section of pangolin.toml. Pass it explicitly so test
        // failures surface useful proxy/lookup debug logs.
        cmd.env("RUST_LOG", "debug");
        cmd.kill_on_drop(true);
        let (child, log, log_tasks) = spawn_with_log_capture(cmd);

        // Wait for all four listeners to bind before returning. The
        // HTTP port is the most reliable readiness signal — pingora
        // logs "Server starting" then binds listeners shortly after.
        // We must also wait for `admin_port` (and the others) because
        // tests that exercise the admin API call `ngx.admin_url(...)`
        // immediately after `start_ngx()`, and hitting a port that the
        // binary hasn't `bind()`'d yet surfaces as a flaky
        // "Connection reset by peer" error.
        //
        // Budget: 30s per port. The previous 5s flaked on CI when
        // cargo test ran many tests in parallel and the OS scheduler
        // starved the cold-starting binaries (see PR #56). Local
        // warm-cache runs finish in <1s, so the extra budget is free.
        // If the binary still fails to boot, the captured log is
        // dumped into the panic message — no more guessing why
        // startup timed out.
        for &port in &[http_port, tls_port, tunnel_port, admin_port] {
            wait_for_port(port, Duration::from_secs(30), &log).await;
        }

        Self {
            child: Some(child),
            log_tasks,
            http_port,
            tls_port,
            tunnel_port,
            admin_port,
            data_dir,
            config_path,
            cert_dir,
            log,
            _tmpdir: tmpdir,
        }
    }

    /// Admin API URL. Always `127.0.0.1:port` (no host override
    /// needed; admin routes by config's `[admin] addr`, not by the
    /// `Host` header).
    pub fn admin_url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.admin_port, path)
    }

    /// Cert directory used by the running `pangolin-ngx`. SNI
    /// callback loads blobs from here. Use [`gen_self_signed`] to
    /// install a per-host cert before connecting with TLS+SNI.
    pub fn cert_dir(&self) -> PathBuf {
        self.cert_dir.clone()
    }

    /// Path to the SQLite database backing this `pangolin-ngx`. Lives
    /// in the per-test tempdir (NOT `data_dir`; the binary writes
    /// `pangolin.db` directly to its CWD, which the harness sets to
    /// `tmpdir`, see `start()` above). Each test gets its own DB and
    /// can poke rows directly via rusqlite without interfering with
    /// the running process (the process opens the same file with WAL).
    pub fn db_path(&self) -> PathBuf {
        // data_dir is `tmpdir/data`; pangolin.db is `tmpdir/pangolin.db`.
        // Walk up one level to land on tmpdir.
        self.data_dir
            .parent()
            .expect("data_dir under tmpdir")
            .join("pangolin.db")
    }

    /// Drain the captured log into a String for diagnostic asserts.
    pub fn log_string(&self) -> String {
        String::from_utf8_lossy(&self.log.lock().unwrap()).into_owned()
    }
}

impl Drop for NgxProcess {
    fn drop(&mut self) {
        // Release allocated ports back to the pool so other tests can use them
        release_port(self.http_port);
        release_port(self.tls_port);
        release_port(self.tunnel_port);
        release_port(self.admin_port);

        // Abort the log-reader tasks first. They hold references to
        // the child's stdout/stderr pipes, so if we let the runtime
        // shut down with them still alive the test process hangs
        // forever. The child is already being killed below, so
        // aborting them is safe — any data not yet drained is lost,
        // and a 1h CI hang is much worse than a missing log line.
        for h in self.log_tasks.drain(..) {
            h.abort();
        }

        let Some(mut child) = self.child.take() else {
            return;
        };
        // Best-effort graceful shutdown: SIGTERM, wait up to 2s, then
        // SIGKILL. We shell out to `kill` for SIGTERM because
        // `Child::kill` (and `start_kill`) only do SIGKILL.
        if let Some(pid) = child.id() {
            let _ = std::process::Command::new("kill")
                .arg("-TERM")
                .arg(pid.to_string())
                .status();
        }
        for _ in 0..40 {
            if child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // Drop runs on a sync stack — `child.kill().await` is not
        // available here. `start_kill()` issues SIGKILL synchronously;
        // the kernel reaps the child via the `try_wait` poll above on
        // any future iteration, but on Drop we are at end-of-life so
        // a brief unreaped zombie is acceptable.
        let _ = child.start_kill();
    }
}

/// RAII handle for a running `pangolin-tun` subprocess. On drop the
/// child is SIGTERM'd then SIGKILL'd, same lifecycle as `NgxProcess`.
pub struct TunProcess {
    pub child: Option<Child>,
    pub name: String,
    pub log: Arc<Mutex<Vec<u8>>>,
    log_tasks: Vec<JoinHandle<()>>,
    // Hold the tempdir so it isn't dropped (and deleted) before the
    // child finishes reading the config it points at.
    _config_tmpdir: TempDir,
}

impl TunProcess {
    /// Spawn `pangolin-tun` pointing at the given ngx instance.
    /// Waits ~1s for the WS connection to complete before returning
    /// (tun logs "tun X connected to ngx" on success; if the auth
    /// fails the log captures the rejection).
    pub async fn start(ngx: &NgxProcess, name: &str, token: &str) -> Self {
        let bin = tun_binary();
        if !bin.exists() {
            panic!(
                "pangolin-tun binary not found at {}. Run `make build` first.",
                bin.display()
            );
        }
        // tun now reads its config from `tun.yml` (the old
        // `--server` / `--name` / `--token` CLI args were removed
        // when the configs were split). The address here is the
        // **tunnel** port (where ngx's WS tunnel server listens),
        // not the admin port.
        let server = format!("127.0.0.1:{}", ngx.tunnel_port);
        let tun_config = format!(
            r#"
server: {server}
name: {name}
token: {token}
log:
  level: debug
"#,
            server = server,
            name = name,
            token = token,
        );
        let tmpdir = tempfile::tempdir().expect("tempdir for tun config");
        let config_path = tmpdir.path().join("tun.yml");
        std::fs::write(&config_path, &tun_config).expect("write tun.yml");
        // tmpdir is moved into Self below so it lives as long as the
        // child process (and is dropped after the child is killed in
        // `Drop`).

        let mut cmd = Command::new(&bin);
        cmd.arg("--config").arg(&config_path);
        cmd.kill_on_drop(true);
        // Strip HTTP_PROXY/HTTPS_PROXY/all_proxy/NO_PROXY: the tun's
        // reqwest client honors them by default, and a user with a
        // SOCKS/HTTP proxy configured in their shell would otherwise
        // see tunneled backend requests routed through the proxy
        // (e.g. `127.0.0.1:1087`), making the test fail with
        // ECONNREFUSED in environments that look fine on the surface.
        // The tun is supposed to make DIRECT outbound connections
        // to the configured backend, exactly as production expects.
        cmd.env_remove("HTTP_PROXY");
        cmd.env_remove("HTTPS_PROXY");
        cmd.env_remove("http_proxy");
        cmd.env_remove("https_proxy");
        cmd.env_remove("ALL_PROXY");
        cmd.env_remove("all_proxy");
        cmd.env_remove("NO_PROXY");
        cmd.env_remove("no_proxy");
        let (child, log, log_tasks) = spawn_with_log_capture(cmd);

        // Tun's own "yamux session live" log is the cleanest readiness
        // signal — it fires AFTER the WS upgrade succeeded AND the
        // yamux control session is open, which is exactly the
        // moment the proxy side will start honouring
        // `app.tun_sessions[name]`. Earlier we waited for
        // "connected to ngx", which fires after the TCP/WS
        // handshake but before the yamux open-stream exchange —
        // on cold CI runners that gap is enough for the test's first
        // request to race ahead and hit "Tun <name> not online" (503).
        //
        // The "yamux session live" marker is emitted by the tun client
        // after the control-stream handshake completes; see
        // `crates/tun/src/client.rs::run_session`.
        //
        // 5s → 15s deadline: cold-cache CI runners routinely take 6-8s
        // to reach this state (DNS + cert verify + dynamic loader +
        // page cache warm-up for both binaries). 15s is well under the
        // e2e job's 15-minute overall budget and well above the
        // observed warm-cache local finish of <1s.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let log_s = String::from_utf8_lossy(&log.lock().unwrap()).into_owned();
            if log_s.contains("yamux session live") {
                break;
            }
            // Legacy fallback for older tun builds that don't log the
            // "yamux session live" marker. Keep this so a freshly-built
            // test crate running against an old `bin/pangolin-tun` still
            // finds the tun ready.
            if log_s.contains("connected to ngx") && log_s.contains("WS upgrade ok") {
                break;
            }
            // Detect early failure (e.g. auth rejection) by looking
            // for the "disconnected" / "error" markers.
            if log_s.contains("disconnected") || log_s.contains("error") {
                // Give the tun a brief moment to flush, then break so
                // the test can read the log and assert on it.
                tokio::time::sleep(Duration::from_millis(100)).await;
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        Self {
            child: Some(child),
            name: name.to_string(),
            log,
            log_tasks,
            _config_tmpdir: tmpdir,
        }
    }

    pub fn log_string(&self) -> String {
        String::from_utf8_lossy(&self.log.lock().unwrap()).into_owned()
    }
}

impl Drop for TunProcess {
    fn drop(&mut self) {
        // Same rationale as NgxProcess::drop: abort the detached
        // log-reader tasks so they don't keep the runtime alive after
        // we kill the child.
        for h in self.log_tasks.drain(..) {
            h.abort();
        }

        let Some(mut child) = self.child.take() else {
            return;
        };
        if let Some(pid) = child.id() {
            let _ = std::process::Command::new("kill")
                .arg("-TERM")
                .arg(pid.to_string())
                .status();
        }
        for _ in 0..40 {
            if child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // Drop runs on a sync stack — `child.kill().await` is not
        // available here. `start_kill()` issues SIGKILL synchronously;
        // the kernel reaps the child via the `try_wait` poll above on
        // any future iteration, but on Drop we are at end-of-life so
        // a brief unreaped zombie is acceptable.
        let _ = child.start_kill();
    }
}
