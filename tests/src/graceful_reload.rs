//! Regression tests for the graceful-reload groundwork (issue #233).
//!
//! Three things are pinned here:
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
//! The tests drive the real binary, so they live behind the
//! `integration` feature — the same convention as the rest of the
//! real-binary suite.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

use crate::harness::{
    CapturedLog, NgxProcess, ngx_binary, spawn_with_log_capture, terminate_child,
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
