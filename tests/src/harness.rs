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

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tempfile::TempDir;
use tokio::io::{AsyncReadExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};

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

/// Reserve a free TCP port by binding to `:0` and reading the assigned
/// port. The listener is dropped immediately so the port is released
/// and the test binary can rebind to it.
pub fn free_port() -> u16 {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0 for free port");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);
    port
}

/// Async readiness check — keep trying `TcpStream::connect` until
/// either it succeeds or the timeout elapses.
async fn wait_for_port(port: u16, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "pangolin-ngx did not start listening on port {} within {:?}",
                port, timeout
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Generate a self-signed cert+key pair for the given SANs and write
/// them as `fullchain.pem` and `privkey.pem` into `out_dir`. The
/// format matches what `pangolin-ngx` expects (PEM-encoded cert +
/// PEM-encoded private key, both in the same file structure as
/// Let's Encrypt's `fullchain.pem` / `privkey.pem`).
pub fn gen_self_signed(sans: &[&str], out_dir: &Path) -> (PathBuf, PathBuf) {
    let sans: Vec<String> = sans.iter().map(|s| s.to_string()).collect();
    let cert_key = rcgen::generate_simple_self_signed(sans).expect("generate self-signed cert");
    let cert_pem = cert_key.cert.pem();
    let key_pem = cert_key.signing_key.serialize_pem();

    let cert_path = out_dir.join("fullchain.pem");
    let key_path = out_dir.join("privkey.pem");
    std::fs::write(&cert_path, cert_pem).expect("write fullchain.pem");
    std::fs::write(&key_path, key_pem).expect("write privkey.pem");
    (cert_path, key_path)
}

/// The pangolin SQLite schema. Mirrors `crates/pangolin-core/src/schema.sql`.
/// Inlined here so the e2e harness doesn't need to depend on
/// `pangolin-core`'s internals.
const SCHEMA_SQL: &str = include_str!("../../crates/pangolin-core/src/schema.sql");

/// Create a fresh `pangolin.db` at `path` with the standard schema
/// applied. The test then opens the same path with rusqlite to insert
/// sites/domains/tokens before the binary starts.
pub fn init_pangolin_db(path: &Path) {
    if path.exists() {
        std::fs::remove_file(path).expect("remove existing pangolin.db");
    }
    let conn = rusqlite::Connection::open(path).expect("open pangolin.db for seeding");
    conn.execute_batch(SCHEMA_SQL).expect("apply schema");
}

/// Capture child stdout+stderr into a shared `Vec<u8>` so failing tests
/// can dump the binary's log output.
fn spawn_with_log_capture(mut cmd: Command) -> (Child, Arc<Mutex<Vec<u8>>>) {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn child process");
    let log = Arc::new(Mutex::new(Vec::<u8>::new()));
    if let Some(stdout) = child.stdout.take() {
        let log = log.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => log.lock().unwrap().extend_from_slice(&buf[..n]),
                    Err(_) => break,
                }
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        let log = log.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => log.lock().unwrap().extend_from_slice(&buf[..n]),
                    Err(_) => break,
                }
            }
        });
    }
    (child, log)
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
    pub log: Arc<Mutex<Vec<u8>>>,
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
        let cert_dir = tmpdir.path().join("certs").join("default");
        std::fs::create_dir_all(&cert_dir).expect("mkdir certs/default");
        gen_self_signed(&["localhost", "127.0.0.1"], &cert_dir);

        let http_port = free_port();
        let tls_port = free_port();
        let tunnel_port = free_port();
        let admin_port = free_port();

        let config = format!(
            r#"
[server]
port = {http}
tls_port = {tls}
tunnel_port = {tunnel_port}
host = "default"

[log]
level = "info"

[admin]
addr = "127.0.0.1:{admin}"

[cert]
cert_dir = "{cert_dir}"
email = ""
autorenew = false
"#,
            http = http_port,
            tls = tls_port,
            tunnel_port = tunnel_port,
            admin = admin_port,
            cert_dir = cert_dir.display().to_string().replace('\\', "\\\\"),
        );
        let config_path = tmpdir.path().join("pangolin.toml");
        // we keep the `cert_dir` in scope (above) so the test's Drop
        // impl that points at the cert path doesn't race the tmpdir
        // teardown. _cert_dir is a no-op; just suppress the warning.
        std::fs::write(&config_path, config).expect("write pangolin.toml");

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
        let (child, log) = spawn_with_log_capture(cmd);

        // The HTTP port is the most reliable readiness signal —
        // pingora logs "Server starting" then binds listeners shortly
        // after. 5s is plenty for a warm cache; if the binary fails to
        // boot (bad config, panic), we'll see it in the captured log
        // via the panic message from `wait_for_port`.
        wait_for_port(http_port, Duration::from_secs(5)).await;

        Self {
            child: Some(child),
            http_port,
            tls_port,
            tunnel_port,
            admin_port,
            data_dir,
            config_path,
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

    /// Drain the captured log into a String for diagnostic asserts.
    pub fn log_string(&self) -> String {
        String::from_utf8_lossy(&self.log.lock().unwrap()).into_owned()
    }
}

impl Drop for NgxProcess {
    fn drop(&mut self) {
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
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// RAII handle for a running `pangolin-tun` subprocess. On drop the
/// child is SIGTERM'd then SIGKILL'd, same lifecycle as `NgxProcess`.
pub struct TunProcess {
    pub child: Option<Child>,
    pub name: String,
    pub log: Arc<Mutex<Vec<u8>>>,
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
        // tun's `--server` is documented as `host:port` (e.g.
        // `ngx.example.com:8080`); the binary naively formats
        // `ws://{server}/tunnel?...`, so passing `http://...` would
        // produce the malformed URL `ws://http://...`. Strip any
        // scheme to keep tun happy.
        //
        // The address here is the **tunnel** port (where ngx's WS
        // tunnel server listens), not the admin port.
        let server = format!("127.0.0.1:{}", ngx.tunnel_port);
        let mut cmd = Command::new(&bin);
        cmd.arg("--server")
            .arg(&server)
            .arg("--name")
            .arg(name)
            .arg("--token")
            .arg(token);
        cmd.kill_on_drop(true);
        let (child, log) = spawn_with_log_capture(cmd);

        // Tun's own "connected to ngx" log is the cleanest readiness
        // signal. We poll the log buffer for up to 5s rather than
        // sleeping a fixed duration.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let log_s = String::from_utf8_lossy(&log.lock().unwrap()).into_owned();
            if log_s.contains("connected to ngx") {
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
        }
    }

    pub fn log_string(&self) -> String {
        String::from_utf8_lossy(&self.log.lock().unwrap()).into_owned()
    }
}

impl Drop for TunProcess {
    fn drop(&mut self) {
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
        let _ = child.kill();
        let _ = child.wait();
    }
}
