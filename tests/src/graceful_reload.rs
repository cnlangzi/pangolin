//! Regression tests for the graceful reload (issues #233 and #234).
//!
//! The groundwork (issue #233):
//!
//! * **`SO_REUSEPORT` on every listener** — two generations of the
//!   gateway hold the same ports at the same time, and both of them
//!   answer on the shared HTTP port.
//! * **`pangolin-ngx --check`** — the exit-code matrix: a valid
//!   configuration says nothing at all and exits 0, a broken one
//!   exits non-zero and names the problem on stderr.
//! * **the PID file** — the gateway writes it before it binds any
//!   listener and removes it when it exits.
//!
//! The handover (issue #234):
//!
//! * **a handover replaces the generation** — `SIGUSR2` starts a new
//!   generation, and the requests keep being answered throughout.
//! * **the new configuration takes effect** — the incoming generation
//!   reads the configuration file again.
//! * **a handover that cannot work changes nothing** — the running
//!   generation keeps its pid and keeps serving.
//! * **an in-flight request is finished** — the drain gives the
//!   requests it took their budget.
//! * **the replaced binary is picked up** — the generation is started
//!   from `argv[0]`, so it runs the file that is there now.
//! * **Ctrl-C stops both processes** — the supervisor forwards the
//!   signal and waits for the generation.
//!
//! The tests drive the real binary, so they live behind the
//! `integration` feature — the same convention as the rest of the
//! real-binary suite.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::Connection;
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

use crate::harness::{
    CapturedLog, NgxProcess, free_port, init_pangolin_db, ngx_binary, raw_request,
    spawn_with_log_capture, terminate_child,
};

/// How long a test waits for the second gateway to bind the shared
/// ports.
const BIND_TIMEOUT: Duration = Duration::from_secs(30);

/// A second `pangolin-ngx` that shares the configuration, the working
/// directory and the ports of an existing one.
///
/// The handover case (issue #233): the incoming generation binds the
/// ports the outgoing one still holds. That only works when every
/// listener asks for `SO_REUSEPORT` — without it the second bind
/// fails with `EADDRINUSE` and the process is out of the game.
struct PeerGateway {
    child: Option<Child>,
    log: CapturedLog,
    log_tasks: Vec<JoinHandle<()>>,
    // The peer's own working directory: it holds its **own**
    // `pangolin.db`, so the two generations never write to the same
    // SQLite file (the database is opened for writing, and
    // `db::open` sets no busy timeout — a concurrent write from the
    // other generation would fail with `SQLITE_BUSY`).
    _work_dir: tempfile::TempDir,
}

impl PeerGateway {
    /// Spawn the second generation next to `first`.
    ///
    /// Same configuration file, same working directory, same ports —
    /// only the process is new.
    async fn start(first: &NgxProcess) -> Self {
        let bin = ngx_binary();
        assert!(
            bin.exists(),
            "pangolin-ngx binary not found at {}. Run `make build` first.",
            bin.display()
        );

        // The peer's own working directory — and therefore its own
        // database. The configuration file is shared (the same
        // ports, the same certificates, the same log level), which is
        // what the handover looks like; the state directory is not.
        let work_dir = tempfile::TempDir::new().expect("tempdir for the peer gateway");

        let mut cmd = Command::new(&bin);
        cmd.arg("--config").arg(&first.config_path);
        cmd.current_dir(work_dir.path());
        // Info everywhere, debug for the proxy module only: the
        // per-request line is what tells the two generations apart,
        // and the debug noise of the background refreshes would keep
        // the log reader busy for no gain.
        cmd.env("RUST_LOG", "info,ngx::proxy=debug");
        cmd.kill_on_drop(true);
        // Strip the proxy variables — the same reason the harness
        // strips them: an ambient SOCKS proxy would break the
        // gateway's own outbound calls.
        cmd.env_remove("HTTP_PROXY");
        cmd.env_remove("HTTPS_PROXY");
        cmd.env_remove("http_proxy");
        cmd.env_remove("https_proxy");
        cmd.env_remove("ALL_PROXY");
        cmd.env_remove("all_proxy");
        cmd.env_remove("NO_PROXY");
        cmd.env_remove("no_proxy");

        let (child, log, log_tasks) = spawn_with_log_capture(cmd);
        Self {
            child: Some(child),
            log,
            log_tasks,
            _work_dir: work_dir,
        }
    }

    /// The captured stdout + stderr of the process.
    fn log_string(&self) -> String {
        String::from_utf8_lossy(&self.log.lock().unwrap()).into_owned()
    }

    /// Is the process still running?
    fn is_running(&mut self) -> bool {
        self.child
            .as_mut()
            .and_then(|c| c.try_wait().ok().flatten())
            .is_none()
    }

    /// Wait until the process logged `needle`.
    async fn wait_for_log(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if self.log_string().contains(needle) {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for PeerGateway {
    fn drop(&mut self) {
        terminate_child(&mut self.child, &mut self.log_tasks);
    }
}

/// The number of requests one probe round sends.
///
/// Enough for the kernel to spread them over the two sockets of the
/// reuseport group; the test keeps going until both generations have
/// answered, so the number only has to make a round useful.
const PROBES_PER_ROUND: usize = 8;

/// How long the test keeps probing for the second generation.
const PROBE_DEADLINE: Duration = Duration::from_secs(30);

/// Two generations of the gateway hold the same ports at the same
/// time, and both of them answer on the shared HTTP port.
///
/// This is the acceptance criterion of issue #233: the kernel spreads
/// the incoming connections over the sockets of the reuseport group
/// by the hash of each connection's 4-tuple, so the outgoing
/// generation can drain while the incoming one already serves.
///
/// Without `SO_REUSEPORT` the second generation loses the race for
/// the ports (`EADDRINUSE`) and never serves a request.
#[tokio::test]
async fn two_gateways_hold_the_same_ports_and_both_serve() {
    let first = NgxProcess::start(|_| {}).await;

    // The challenge files the ACME HTTP-01 path answers from. That
    // path is the one that returns 200 without a backend — and both
    // generations read the same cert dir, so only their own logs tell
    // which one answered.
    let challenge_dir = first.cert_dir().join(".well-known").join("acme-challenge");
    std::fs::create_dir_all(&challenge_dir).expect("create the challenge dir");

    let mut second = PeerGateway::start(&first).await;

    // The second generation bound its own listeners. The tunnel
    // listener logs after its bind, so the line is the proof that the
    // shared ports were taken — not refused.
    assert!(
        second
            .wait_for_log("tunnel server listening on", BIND_TIMEOUT)
            .await,
        "the second gateway did not bind the shared ports:\n{}",
        second.log_string()
    );
    assert!(
        second.is_running(),
        "the second gateway exited:\n{}",
        second.log_string()
    );

    // Every request to the shared HTTP port is answered with 200,
    // and **both** generations have to answer: the kernel spreads the
    // connections over the sockets of the reuseport group.
    //
    // The loop keeps probing until the two of them have answered
    // between them. The second generation binds its HTTP listener a
    // moment after its other listeners, and a probe that arrives
    // before that is answered by the first generation alone — which
    // would look like a gateway that never joined the group.
    let addr = format!("127.0.0.1:{}", first.http_port);
    let mut tokens: Vec<String> = Vec::new();
    let deadline = std::time::Instant::now() + PROBE_DEADLINE;
    loop {
        for probe in 0..PROBES_PER_ROUND {
            let token = format!("reuseport-{probe}");
            std::fs::write(challenge_dir.join(&token), token.as_bytes())
                .expect("write the challenge file");
            let path = format!("/.well-known/acme-challenge/{token}");
            let (status, body) = probe_request(&addr, &path).await;
            assert_eq!(status, 200, "the shared HTTP port answered {status}");
            assert_eq!(body, token, "the challenge body came back");
            if !tokens.contains(&token) {
                tokens.push(token);
            }
        }

        // Both generations have to have served at least one request:
        // the per-request log line is the only thing that tells the
        // two apart, because they share the configuration, the
        // database and the certificates.
        if first_served(&first, &tokens) && second_served(&second, &tokens) {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "the two generations did not both serve:\n--- first ---\n{}\n--- second ---\n{}",
                first.log_string(),
                second.log_string()
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// One probe request against the shared HTTP port, with the retries.
///
/// A connection can be reset while the second socket joins the
/// reuseport group — the kernel re-spreads the connections that are
/// already up — so a probe that does not make it is retried rather
/// than failed.
async fn probe_request(addr: &str, path: &str) -> (u16, String) {
    for _ in 0..10 {
        if let Some(answer) = try_probe(addr, path).await {
            return answer;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the shared HTTP port {addr} did not answer {path}");
}

/// One probe request, without the retries.
///
/// `None` when the connection did not make it.
async fn try_probe(addr: &str, path: &str) -> Option<(u16, String)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream =
        tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(addr))
            .await
            .ok()?
            .ok()?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: probe.example.com\r\nConnection: close\r\n\
         User-Agent: Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
         Chrome/120.0.0.0 Safari/537.36\r\nAccept: */*\r\nContent-Length: 0\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.ok()?;
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf))
        .await
        .ok()?
        .ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())?;
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    Some((status, body))
}

/// Did the first gateway serve one of the probe requests?
///
/// The gateway logs every request it takes in, and the probe paths
/// are unique — so a path in the log is a request this process
/// answered.
fn first_served(ngx: &NgxProcess, tokens: &[String]) -> bool {
    let log = ngx.log_string();
    tokens.iter().any(|t| log.contains(t.as_str()))
}

/// Did the second gateway serve one of the probe requests?
fn second_served(peer: &PeerGateway, tokens: &[String]) -> bool {
    let log = peer.log_string();
    tokens.iter().any(|t| log.contains(t.as_str()))
}

/// Run `pangolin-ngx --check` and return its exit code, stdout and
/// stderr.
async fn run_check(config_path: &Path, work_dir: &Path) -> (i32, Vec<u8>, Vec<u8>) {
    let bin = ngx_binary();
    assert!(
        bin.exists(),
        "pangolin-ngx binary not found at {}. Run `make build` first.",
        bin.display()
    );
    let output = Command::new(&bin)
        .arg("--config")
        .arg(config_path)
        .arg("--check")
        .current_dir(work_dir)
        .output()
        .await
        .expect("run pangolin-ngx --check");
    (
        output.status.code().unwrap_or(-1),
        output.stdout,
        output.stderr,
    )
}

/// A configuration the check accepts.
///
/// The cert dir has to exist — the check refuses a missing one, and
/// the operator creates it before the check.
fn write_valid_config(dir: &Path, cert_dir: &Path) -> PathBuf {
    std::fs::create_dir_all(cert_dir).expect("create the cert dir");
    let config = format!(
        "addr:\n  http: \"127.0.0.1:0\"\n  https: \"127.0.0.1:0\"\n\nacme:\n  cert_dir: \"{}\"\n",
        cert_dir.display()
    );
    let path = dir.join("ngx.yml");
    std::fs::write(&path, config).expect("write ngx.yml");
    path
}

/// A valid configuration exits 0 and says nothing at all.
///
/// The exit code is the answer: a check that prints its verdict on
/// stdout would be parsed by whatever runs it, and a check that
/// prints a log line would make the operator read the log to find
/// out whether the check passed.
#[tokio::test]
async fn the_check_exits_zero_and_says_nothing_on_a_valid_configuration() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let config_path = write_valid_config(dir.path(), &dir.path().join("certs"));

    let (code, stdout, stderr) = run_check(&config_path, dir.path()).await;

    assert_eq!(code, 0, "a valid configuration passes the check");
    assert!(
        stdout.is_empty(),
        "a successful check prints nothing on stdout: {}",
        String::from_utf8_lossy(&stdout)
    );
    assert!(
        stderr.is_empty(),
        "a successful check prints nothing on stderr: {}",
        String::from_utf8_lossy(&stderr)
    );
}

/// A configuration that does not parse exits non-zero and names the
/// problem.
#[tokio::test]
async fn the_check_exits_non_zero_on_a_configuration_that_does_not_parse() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let config_path = dir.path().join("ngx.yml");
    // An unclosed flow sequence — a YAML syntax error, not a
    // configuration mistake.
    std::fs::write(&config_path, "addr: [unterminated\n").expect("write the broken ngx.yml");

    let (code, _stdout, stderr) = run_check(&config_path, dir.path()).await;

    assert_ne!(code, 0, "a configuration that does not parse fails");
    let stderr = String::from_utf8_lossy(&stderr);
    assert!(
        stderr.contains("config"),
        "the error has to name the configuration: {stderr}"
    );
}

/// A configuration whose cert dir is not there exits non-zero and
/// names the cert dir.
///
/// The check refuses a missing cert dir instead of creating it: the
/// real startup creates the directory, but a check that creates what
/// it is about to complain about is no check at all.
#[tokio::test]
async fn the_check_exits_non_zero_when_the_cert_dir_is_missing() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let cert_dir = dir.path().join("certs");
    let config_path = write_valid_config(dir.path(), &cert_dir);
    // Take the directory away again — the check must not create it.
    std::fs::remove_dir_all(&cert_dir).expect("remove the cert dir");

    let (code, _stdout, stderr) = run_check(&config_path, dir.path()).await;

    assert_ne!(code, 0, "a missing cert dir fails the check");
    let stderr = String::from_utf8_lossy(&stderr);
    assert!(
        stderr.contains("cert dir"),
        "the error has to name the cert dir: {stderr}"
    );
    assert!(
        !cert_dir.exists(),
        "the check must not create the cert dir it complained about"
    );
}

/// The gateway writes its PID file before it binds, and removes it
/// when it exits.
///
/// The file is the anchor for the operator commands that address the
/// gateway by its process number.
#[tokio::test]
async fn the_gateway_writes_the_pid_file_and_removes_it_on_exit() {
    let mut ngx = NgxProcess::start(|_| {}).await;

    // The path is derived from the configuration file's own path:
    // `<work dir>/ngx.yml` → `<work dir>/ngx.pid`.
    let pid_file = ngx.work_dir().join("ngx.pid");
    assert!(
        pid_file.exists(),
        "the gateway did not write the pid file:\n{}",
        ngx.log_string()
    );

    let content = std::fs::read_to_string(&pid_file).expect("read the pid file");
    let pid: u32 = content
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("the pid file does not hold a number: {content:?} ({e})"));
    assert_eq!(
        pid,
        ngx.child
            .as_ref()
            .expect("the child")
            .id()
            .expect("the pid") as u32,
        "the pid file holds the number of the running gateway"
    );

    // A graceful exit takes the file with it.
    let child_pid = ngx
        .child
        .as_ref()
        .expect("the child")
        .id()
        .expect("the pid");
    unsafe {
        libc::kill(child_pid as i32, libc::SIGTERM);
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        if ngx
            .child
            .as_mut()
            .and_then(|c| c.try_wait().ok().flatten())
            .is_some()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Give the process a moment to remove the file after it exits.
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert!(
        !pid_file.exists(),
        "the graceful exit did not remove the pid file"
    );
}

// ---------------------------------------------------------------------------
// The handover (issue #234)
// ---------------------------------------------------------------------------

/// The pid of the generation that answers the admin's health
/// endpoint.
///
/// The gateway runs as two processes: the supervisor and the
/// generation that serves the traffic. The health endpoint answers
/// with the pid of the process that served it, which is how a test
/// tells the two apart.
async fn generation_pid(admin_port: u16) -> Option<u32> {
    let addr = format!("127.0.0.1:{admin_port}");
    // `try_probe`, not `raw_request`: the latter panics on a refused
    // connection, and this helper polls a port that may not be up
    // yet.
    let (status, body) = try_probe(&addr, "/healthz").await?;
    if status != 200 {
        return None;
    }
    let parsed: serde_json::Value = serde_json::from_str(&body).ok()?;
    parsed.get("pid")?.as_u64().map(|pid| pid as u32)
}

/// Wait until the generation that answers the health endpoint is a
/// different one.
async fn wait_for_a_new_generation(admin_port: u16, before: u32) -> u32 {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(pid) = generation_pid(admin_port).await
            && pid != before
        {
            return pid;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the generation was not replaced within 30s"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A path the proxy answers with 200 and the file's content.
///
/// The ACME challenge path is the one the gateway answers from the
/// certificate directory without a backend — so it is a real proxy
/// request that needs no site, and both generations answer it (they
/// share the certificate directory).
fn install_challenge(cert_dir: &Path, token: &str) -> String {
    let dir = cert_dir.join(".well-known").join("acme-challenge");
    std::fs::create_dir_all(&dir).expect("create the challenge dir");
    std::fs::write(dir.join(token), token.as_bytes()).expect("write the challenge file");
    format!("/.well-known/acme-challenge/{token}")
}

/// The supervisor's process number — the process the harness spawned.
fn supervisor_pid(ngx: &NgxProcess) -> u32 {
    ngx.child
        .as_ref()
        .expect("the child")
        .id()
        .expect("the pid")
}

/// Ask the gateway to hand over to a new generation.
///
/// `SIGUSR2` is what the systemd unit's `ExecReload` sends.
fn request_handover(supervisor: u32) {
    // SAFETY: `SIGUSR2` is the handover signal and the pid is the
    // supervisor's own.
    let rc = unsafe { libc::kill(supervisor as i32, libc::SIGUSR2) };
    assert_eq!(
        rc,
        0,
        "kill(SIGUSR2) returned {rc} ({})",
        std::io::Error::last_os_error()
    );
}

/// A handover replaces the generation, and the requests keep working.
///
/// The acceptance criterion of issue #234: the port is held
/// throughout, so a request that arrives during the handover is
/// answered — by the incoming generation.
#[tokio::test]
async fn a_handover_replaces_the_generation_without_a_gap() {
    let ngx = NgxProcess::start(|_| {}).await;
    let supervisor = supervisor_pid(&ngx);
    let before = generation_pid(ngx.admin_port)
        .await
        .expect("the gateway reports the generation's pid");
    assert_ne!(
        before, supervisor,
        "the supervisor and the generation are two processes"
    );

    let path = install_challenge(&ngx.cert_dir(), "handover");
    let addr = format!("127.0.0.1:{}", ngx.http_port);

    // The prober: a request every 20 ms, recording every answer
    // that is not a 200.
    let failures = Arc::new(Mutex::new(Vec::<String>::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let prober = {
        let addr = addr.clone();
        let failures = failures.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            while !stop.load(Ordering::SeqCst) {
                let answer = try_probe(&addr, &path).await;
                let failure = match answer {
                    Some((status, body)) if status == 200 => None,
                    Some((status, body)) => Some(format!("{status}: {body}")),
                    None => Some("the connection was refused".to_string()),
                };
                if let Some(failure) = failure {
                    failures.lock().expect("the failure log").push(failure);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
    };

    request_handover(supervisor);

    let after = wait_for_a_new_generation(ngx.admin_port, before).await;
    assert_ne!(after, before, "the generation must be replaced");

    // Let the prober run a while into the new generation.
    tokio::time::sleep(Duration::from_millis(500)).await;
    stop.store(true, Ordering::SeqCst);
    prober.await.expect("the prober joins");

    let failures = failures.lock().expect("the failure log");
    assert!(
        failures.is_empty(),
        "no request may fail during the handover: {failures:?}\nngx log:\n{}",
        ngx.log_string()
    );
}

/// The handover picks up the configuration that changed on disk.
///
/// The incoming generation reads the configuration file again, so a
/// change to it takes effect. Here: the admin moves to another port.
#[tokio::test]
async fn a_handover_picks_up_the_new_configuration() {
    let ngx = NgxProcess::start(|_| {}).await;
    let supervisor = supervisor_pid(&ngx);
    let before = generation_pid(ngx.admin_port)
        .await
        .expect("the gateway reports the generation's pid");

    // Move the admin to another port.
    let new_admin_port = free_port();
    let config = std::fs::read_to_string(&ngx.config_path).expect("read the config");
    let config = config.replace(
        &format!("addr: 127.0.0.1:{}", ngx.admin_port),
        &format!("addr: 127.0.0.1:{new_admin_port}"),
    );
    std::fs::write(&ngx.config_path, config).expect("write the config");

    request_handover(supervisor);

    // The new admin port answers — with the new generation.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut pid = None;
    while std::time::Instant::now() < deadline {
        if let Some(p) = generation_pid(new_admin_port).await {
            pid = Some(p);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let pid = pid.expect("the new admin port must answer after the handover");
    assert_ne!(
        pid, before,
        "the new configuration must be served by the new generation"
    );
}

/// A handover that cannot work changes nothing.
///
/// The new configuration does not parse, so the incoming generation
/// dies during its startup. The running generation never stopped
/// accepting: its pid is unchanged and it keeps answering.
#[tokio::test]
async fn a_failed_handover_leaves_the_running_gateway_alone() {
    let ngx = NgxProcess::start(|_| {}).await;
    let supervisor = supervisor_pid(&ngx);
    let before = generation_pid(ngx.admin_port)
        .await
        .expect("the gateway reports the generation's pid");
    let path = install_challenge(&ngx.cert_dir(), "failed-handover");
    let addr = format!("127.0.0.1:{}", ngx.http_port);

    // Break the configuration.
    std::fs::write(&ngx.config_path, "addr: [unterminated\n").expect("break the config");

    request_handover(supervisor);

    // The supervisor says what went wrong.
    assert!(
        crate::harness::wait_for_log(&ngx.log, "the handover failed", Duration::from_secs(30))
            .await,
        "the supervisor must report the failed handover:\n{}",
        ngx.log_string()
    );

    // The generation is untouched and still serving.
    let after = generation_pid(ngx.admin_port)
        .await
        .expect("the generation still answers");
    assert_eq!(after, before, "the running generation must not be replaced");
    let (status, body) = try_probe(&addr, &path)
        .await
        .expect("the running gateway must keep serving");
    assert_eq!(
        status,
        200,
        "the running gateway must keep serving: {body}\nngx log:\n{}",
        ngx.log_string()
    );
}

/// A backend that answers after a delay.
///
/// The delay is what makes a request "in flight" long enough for the
/// handover to happen while it is running.
struct SlowBackend {
    addr: String,
    handle: tokio::task::JoinHandle<()>,
}

impl SlowBackend {
    async fn start(delay: Duration) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the backend");
        let addr = listener.local_addr().expect("the address").to_string();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 4096];
                    let _ = stream.read(&mut buf).await;
                    tokio::time::sleep(delay).await;
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        )
                        .await;
                });
            }
        });
        Self { addr, handle }
    }
}

impl Drop for SlowBackend {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// A request that is in flight when the handover starts is finished.
///
/// The drain gives the in-flight requests their budget before the
/// generation goes.
#[tokio::test]
async fn an_in_flight_request_survives_the_handover() {
    let backend = SlowBackend::start(Duration::from_millis(1500)).await;
    let backend_addr = backend.addr.clone();

    let ngx = NgxProcess::start(move |db_path| {
        init_pangolin_db(db_path);
        let conn = Connection::open(db_path).expect("open the database");
        seed_site(&conn, "slow-site", &format!("http://{backend_addr}"));
        seed_domain(&conn, "slow.test", "slow-site");
    })
    .await;
    let supervisor = supervisor_pid(&ngx);
    let addr = format!("127.0.0.1:{}", ngx.http_port);

    // The request goes out and the backend takes its time.
    let request = tokio::spawn({
        let addr = addr.clone();
        async move { raw_request(&addr, "slow.test", "GET", "/", b"").await }
    });
    tokio::time::sleep(Duration::from_millis(400)).await;

    // The handover happens while the request is in flight.
    request_handover(supervisor);

    let (status, body) = request.await.expect("the request finishes");
    assert_eq!(
        status,
        200,
        "the in-flight request must be finished, got {status}: {body}\nngx log:\n{}",
        ngx.log_string()
    );
}

/// Seed a site that points at `backend`.
fn seed_site(conn: &Connection, name: &str, backend: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO sites (name, backend, enabled, host_mode, host_custom, created_at, updated_at)
         VALUES (?1, ?2, 1, 'passthrough', NULL, ?3, ?3)",
        rusqlite::params![name, backend, now],
    )
    .expect("insert the site");
}

/// Seed a domain that points at `site_name`.
fn seed_domain(conn: &Connection, domain: &str, site_name: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO domains (domain, site_name, enabled, created_at) VALUES (?1, ?2, 1, ?3)",
        rusqlite::params![domain, site_name, now],
    )
    .expect("insert the domain");
}

/// A gateway the test starts from a **copy** of the binary.
///
/// The operator replaces the binary with an atomic `mv` and then
/// reloads. The generation is started from `argv[0]`, so it runs
/// whatever is at that path — the new file.
struct ReplaceableGateway {
    child: Option<Child>,
    /// The path the gateway is started from.
    binary: PathBuf,
    admin_port: u16,
    log: CapturedLog,
    log_tasks: Vec<JoinHandle<()>>,
    _work_dir: tempfile::TempDir,
}

impl ReplaceableGateway {
    /// Start a gateway from a copy of the binary.
    async fn start() -> Self {
        let bin = ngx_binary();
        assert!(
            bin.exists(),
            "pangolin-ngx binary not found at {}. Run `make build` first.",
            bin.display()
        );

        let work_dir = tempfile::TempDir::new().expect("tempdir");
        let binary = work_dir.path().join("pangolin-ngx");
        std::fs::copy(&bin, &binary).expect("copy the binary");

        let http_port = free_port();
        let tls_port = free_port();
        let tunnel_port = free_port();
        let admin_port = free_port();
        let api_port = free_port();
        let cert_dir = work_dir.path().join("certs");
        std::fs::create_dir_all(&cert_dir).expect("mkdir the cert dir");

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

api:
  listen: 127.0.0.1:{api}

reload:
  ready_timeout_secs: 5
  drain_timeout_secs: 5

acme:
  cert_dir: "{cert_dir}"
  email: ""
"#,
            http = http_port,
            tls = tls_port,
            tunnel_port = tunnel_port,
            admin = admin_port,
            api = api_port,
            cert_dir = cert_dir.display().to_string().replace('\\', "\\\\"),
        );
        let config_path = work_dir.path().join("ngx.yml");
        std::fs::write(&config_path, config).expect("write the config");

        let mut cmd = Command::new(&binary);
        cmd.arg("--config").arg(&config_path);
        cmd.current_dir(work_dir.path());
        cmd.env("RUST_LOG", "info");
        cmd.kill_on_drop(true);
        // Its own process group: a signal to the group is then the
        // signal of a Ctrl-C in the terminal.
        cmd.process_group(0);
        // Strip the proxy variables — the same reason the harness
        // strips them.
        cmd.env_remove("HTTP_PROXY");
        cmd.env_remove("HTTPS_PROXY");
        cmd.env_remove("http_proxy");
        cmd.env_remove("https_proxy");
        cmd.env_remove("ALL_PROXY");
        cmd.env_remove("all_proxy");
        cmd.env_remove("NO_PROXY");
        cmd.env_remove("no_proxy");

        let (child, log, log_tasks) = spawn_with_log_capture(cmd);
        let gateway = Self {
            child: Some(child),
            binary,
            admin_port,
            log,
            log_tasks,
            _work_dir: work_dir,
        };
        gateway.wait_for_the_admin_port().await;
        gateway
    }

    /// The gateway's working directory.
    fn work_dir(&self) -> PathBuf {
        self._work_dir.path().to_path_buf()
    }

    /// Wait until the admin port answers.
    async fn wait_for_the_admin_port(&self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let addr = format!("127.0.0.1:{}", self.admin_port);
            if let Some((200, _)) = try_probe(&addr, "/healthz").await {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the gateway did not answer on the admin port within 30s:\n{}",
                String::from_utf8_lossy(&self.log.lock().expect("the log"))
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

impl Drop for ReplaceableGateway {
    fn drop(&mut self) {
        terminate_child(&mut self.child, &mut self.log_tasks);
    }
}

/// Whether a process is still alive.
fn process_alive(pid: u32) -> bool {
    // SAFETY: signal 0 is the existence check.
    let rc = unsafe { libc::kill(pid as i32, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// A handover picks up the binary that replaced the old one.
///
/// The operator replaces the binary with an atomic `mv`; the
/// generation is started from `argv[0]`, so it runs the file that is
/// there now — not the inode the old generation was started from.
#[tokio::test]
async fn a_handover_picks_up_the_replaced_binary() {
    let gateway = ReplaceableGateway::start().await;
    let supervisor = gateway
        .child
        .as_ref()
        .expect("the child")
        .id()
        .expect("the pid");
    let before = generation_pid(gateway.admin_port)
        .await
        .expect("the gateway reports the generation's pid");

    // The binary as the running generation sees it.
    let inode_before = std::fs::metadata(format!("/proc/{before}/exe"))
        .expect("the binary of the running generation")
        .ino();

    // The operator replaces the binary — an atomic `mv`, the way a
    // package upgrade does it.
    let replacement = gateway.work_dir().join("pangolin-ngx.new");
    std::fs::copy(&gateway.binary, &replacement).expect("copy the new binary");
    std::fs::rename(&replacement, &gateway.binary).expect("replace the binary");

    request_handover(supervisor);

    let after = wait_for_a_new_generation(gateway.admin_port, before).await;
    let inode_after = std::fs::metadata(format!("/proc/{after}/exe"))
        .expect("the binary of the new generation")
        .ino();
    assert_ne!(
        inode_after, inode_before,
        "the new generation must run the replaced binary"
    );
}

/// A Ctrl-C stops the supervisor and the generation.
///
/// The signal of a Ctrl-C goes to the whole foreground process
/// group, so both processes get it. The supervisor forwards it and
/// waits for the generation; the generation drains and exits.
#[tokio::test]
async fn a_ctrl_c_stops_the_supervisor_and_the_generation() {
    let mut gateway = ReplaceableGateway::start().await;
    let supervisor = gateway
        .child
        .as_ref()
        .expect("the child")
        .id()
        .expect("the pid");
    let generation = generation_pid(gateway.admin_port)
        .await
        .expect("the gateway reports the generation's pid");

    // SAFETY: the negative pid is the gateway's own process group —
    // the group the supervisor leads.
    let rc = unsafe { libc::kill(-(supervisor as i32), libc::SIGINT) };
    assert_eq!(
        rc,
        0,
        "kill(SIGINT) returned {rc} ({})",
        std::io::Error::last_os_error()
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let supervisor_alive = gateway
            .child
            .as_mut()
            .and_then(|c| c.try_wait().ok().flatten())
            .is_none();
        let generation_alive = process_alive(generation);
        if !supervisor_alive && !generation_alive {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the processes did not exit within 30s (supervisor alive: {supervisor_alive}, \
             generation alive: {generation_alive})\ngateway log:\n{}",
            String::from_utf8_lossy(&gateway.log.lock().expect("the log"))
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Run `pangolin-ngx <flag>` against the running gateway and return
/// its exit code, stdout and stderr.
async fn run_operator_command(ngx: &NgxProcess, flag: &str) -> (i32, String, String) {
    let bin = ngx_binary();
    assert!(
        bin.exists(),
        "pangolin-ngx binary not found at {}. Run `make build` first.",
        bin.display()
    );
    let output = Command::new(&bin)
        .arg("--config")
        .arg(&ngx.config_path)
        .arg(flag)
        .current_dir(ngx.work_dir())
        .output()
        .await
        .expect("run the operator command");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// `pangolin-ngx --reload` asks the running gateway to hand over.
///
/// The command is asynchronous: it sends the signal and exits, and
/// the handover happens behind it.
#[tokio::test]
async fn the_reload_command_hands_the_gateway_over() {
    let ngx = NgxProcess::start(|_| {}).await;
    let before = generation_pid(ngx.admin_port)
        .await
        .expect("the gateway reports the generation's pid");

    let (code, stdout, stderr) = run_operator_command(&ngx, "--reload").await;
    assert_eq!(code, 0, "the reload command must succeed: {stderr}");
    assert!(
        stdout.contains("SIGUSR2"),
        "the command has to say what it did: {stdout}"
    );

    let after = wait_for_a_new_generation(ngx.admin_port, before).await;
    assert_ne!(after, before, "the generation must be replaced");
}

/// `pangolin-ngx --status` reports the supervisor and the generation.
#[tokio::test]
async fn the_status_command_reports_the_two_processes() {
    let ngx = NgxProcess::start(|_| {}).await;
    let supervisor = supervisor_pid(&ngx);
    let generation = generation_pid(ngx.admin_port)
        .await
        .expect("the gateway reports the generation's pid");

    let (code, stdout, stderr) = run_operator_command(&ngx, "--status").await;
    assert_eq!(code, 0, "the status command must succeed: {stderr}");
    assert!(
        stdout.contains(&supervisor.to_string()),
        "the supervisor's pid must be reported: {stdout}"
    );
    assert!(
        stdout.contains(&generation.to_string()),
        "the generation's pid must be reported: {stdout}"
    );
    assert!(
        stdout.contains("ok"),
        "the generation's state must be reported: {stdout}"
    );
}

/// `--reload` on a gateway that is not running fails and says so.
#[tokio::test]
async fn the_reload_command_fails_when_the_gateway_is_not_running() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let config_path = dir.path().join("ngx.yml");
    // The PID file is derived from the configuration file's own path
    // — and there is none.
    std::fs::write(&config_path, "addr:\n  http: \"127.0.0.1:0\"\n").expect("write the config");

    let bin = ngx_binary();
    let output = Command::new(&bin)
        .arg("--config")
        .arg(&config_path)
        .arg("--reload")
        .current_dir(dir.path())
        .output()
        .await
        .expect("run pangolin-ngx --reload");

    assert_ne!(
        output.status.code().unwrap_or(-1),
        0,
        "the command must fail when the gateway is not running"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not running"),
        "the error has to say that the gateway is not running: {stderr}"
    );
}
