//! Regression tests for the one-line installer (issue #175).
//!
//! The installer is the operator's first contact with a release: one
//! command has to produce a gateway that actually runs — the right
//! binary for the machine, verified against the release's checksum
//! file, a production configuration, a systemd unit and a service that
//! is up.
//!
//! These tests drive the real `install.sh` — the file the release
//! publishes — against a local HTTP server that impersonates a GitHub
//! Release.  Nothing is mocked inside the script: `sh` runs it, `curl`
//! fetches from the fake server, `tar` unpacks a real archive, and the
//! commands that would touch the machine (`uname`, `id`, `chown`,
//! `systemctl`, `useradd`) are replaced by fakes on a private `PATH`.
//!
//! They are deliberately cheap and hermetic: no network, no root, no
//! real release.  So they run in the day-to-day `cargo test --workspace`
//! / `make lint` cycle.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::thread;

use sha2::{Digest, Sha256};
use tempfile::TempDir;

use crate::common::{read_repo_file, repo_root};
use crate::release_version::step_script;

/// The version the fake release publishes.
const RELEASE_VERSION: &str = "v1.2.3";

/// The install directory the repository's systemd units point at.
const DEFAULT_INSTALL_DIR: &str = "/usr/local/pangolin";

// ── the fake release server ─────────────────────────────────────────────────

/// What the fake server answers with.
enum Reply {
    Body(Vec<u8>),
    Redirect(String),
    NotFound,
}

/// The answer for one request path.
type Handler = Box<dyn Fn(&str) -> Reply + Send + Sync>;

/// A one-thread HTTP server that answers with `handler` and records
/// every path it was asked for.
///
/// The paths are the interesting part: the installer asks for exactly
/// one archive, one checksum file and two units, and the *names* of
/// those requests are what the platform and the version tests assert
/// on.
fn spawn_server(handler: Handler) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the fake release");
    let addr = listener.local_addr().expect("the fake release's address");
    let requests: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);

    thread::spawn(move || {
        for incoming in listener.incoming() {
            let Ok(mut stream) = incoming else { continue };
            let Ok(clone) = stream.try_clone() else {
                continue;
            };
            let mut reader = BufReader::new(clone);

            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }
            // Drain the headers — the answer does not depend on them,
            // but the client waits for the request to be complete.
            loop {
                let mut header = String::new();
                match reader.read_line(&mut header) {
                    Ok(0) => break,
                    Ok(_) => {
                        if header.trim().is_empty() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }

            let path = request_line
                .split_whitespace()
                .nth(1)
                .unwrap_or("/")
                .to_string();
            recorded.lock().expect("the request log").push(path.clone());

            let head_and_body = match handler(&path) {
                Reply::Body(body) => {
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let mut out = head.into_bytes();
                    out.extend_from_slice(&body);
                    out
                }
                Reply::Redirect(location) => format!(
                    "HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    location
                )
                .into_bytes(),
                Reply::NotFound => {
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_vec()
                }
            };
            let _ = stream.write_all(&head_and_body);
            let _ = stream.flush();
        }
    });

    (format!("http://{addr}"), requests)
}

/// The SHA-256 of a byte string, in the lowercase hex form
/// `SHA256SUMS.txt` uses.
fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The contents of a `SHA256SUMS.txt` — one `sha256sum` line per file.
fn sums_file(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = String::new();
    for (name, bytes) in entries {
        out.push_str(&format!("{}  {}\n", sha256_hex(bytes), name));
    }
    out.into_bytes()
}

/// The checksum file of a release: it lists the archive of every
/// platform, not just the one this machine downloaded — the installer
/// has to pick its own entry out of it.
fn sums_for_version(version: &str, archive: &[u8], units: &[(&str, &[u8])]) -> Vec<u8> {
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    for target in ["linux_amd64", "linux_arm64", "macos_arm64", "macos_x64"] {
        entries.push((
            format!("pangolin_{version}_{target}.tar.gz"),
            archive.to_vec(),
        ));
    }
    for (name, bytes) in units {
        entries.push(((*name).to_string(), bytes.to_vec()));
    }

    let borrowed: Vec<(&str, &[u8])> = entries
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
        .collect();
    sums_file(&borrowed)
}

/// The archive the release publishes: the two binaries and the two
/// configuration files, at the archive root — exactly the layout the
/// release workflow builds.
///
/// The configuration files are the repository's own, byte for byte, so
/// the "only five lines changed" assertion below is about the real
/// file and not about a fixture.
fn build_archive(stage: &Path) -> Vec<u8> {
    fs::create_dir_all(stage).expect("create the staging directory");
    fs::write(stage.join("pangolin-ngx"), "#!/bin/sh\necho pangolin-ngx\n")
        .expect("write the fake gateway");
    fs::write(stage.join("pangolin-tun"), "#!/bin/sh\necho pangolin-tun\n")
        .expect("write the fake tunnel node");
    for name in ["ngx.yml", "tun.yml"] {
        fs::copy(repo_root().join(name), stage.join(name)).expect("copy the configuration");
    }

    let archive = stage.join("archive.tar.gz");
    let status = Command::new(host_tool("tar").expect("tar"))
        .arg("-C")
        .arg(stage)
        .arg("-czf")
        .arg(&archive)
        .arg("pangolin-ngx")
        .arg("pangolin-tun")
        .arg("ngx.yml")
        .arg("tun.yml")
        .status()
        .expect("run tar");
    assert!(status.success(), "tar failed to build the test archive");

    fs::read(&archive).expect("read the test archive")
}

// ── the sandbox ─────────────────────────────────────────────────────────────

/// A tool of the host, found the way the shell would find it.
fn host_tool(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// The installer is a shell script that downloads with curl and
/// verifies the download with one of the three digest tools.  A host
/// without them cannot run it — the test skips instead of failing, the
/// same rule `make test-js` follows when node is missing.
fn prerequisites_available() -> bool {
    host_tool("sh").is_some()
        && host_tool("curl").is_some()
        && DIGEST_TOOLS.iter().any(|tool| host_tool(tool).is_some())
}

/// The digest tool the sandbox offers by default: the first one the
/// host actually has, in the order the installer prefers them.
fn host_digest_tool() -> &'static str {
    DIGEST_TOOLS
        .iter()
        .find(|tool| host_tool(tool).is_some())
        .copied()
        .expect("the host has one of sha256sum, shasum and openssl")
}

/// The tools the installer shells out to.  They are linked into a
/// private directory so that the test controls exactly which ones
/// exist — that is how the digest-tool fallback is exercised.
const TOOLS: &[&str] = &[
    "awk", "sed", "tr", "cut", "tar", "gzip", "curl", "mkdir", "cp", "mv", "rm", "chmod", "mktemp",
    "cat", "sleep",
];

/// The SHA-256 implementation the host has, in the order the installer
/// prefers them.  macOS has no `sha256sum` — only `shasum` — so the
/// tool is resolved from what the host actually has.
const DIGEST_TOOLS: &[&str] = &["sha256sum", "shasum", "openssl"];

/// The fake `uname`.
const FAKE_UNAME: &str = r#"#!/bin/sh
case "$1" in
  -s) printf '%s\n' "$FAKE_UNAME_S" ;;
  -m) printf '%s\n' "$FAKE_UNAME_M" ;;
  *)  printf '%s\n' "$FAKE_UNAME_S" ;;
esac
"#;

/// The fake `hostname`.
const FAKE_HOSTNAME: &str = r#"#!/bin/sh
printf '%s\n' "$FAKE_HOSTNAME"
"#;

/// The fake `id`.  It answers two questions: "am I root?" and "does
/// the `pangolin` user exist?".
const FAKE_ID: &str = r#"#!/bin/sh
case "$1" in
  -u)
    if [ "$2" = "pangolin" ]; then
      if [ "$FAKE_USER_EXISTS" = "1" ]; then
        printf '%s\n' "$FAKE_USER_UID"
        exit 0
      fi
      printf 'id: pangolin: no such user\n' >&2
      exit 1
    fi
    printf '%s\n' "$FAKE_ID_UID"
    exit 0
    ;;
  -un)
    printf '%s\n' "$FAKE_USER_NAME"
    exit 0
    ;;
  *)
    printf 'uid=%s\n' "$FAKE_ID_UID"
    exit 0
    ;;
esac
"#;

/// The fake `chown` — it records the call instead of changing a file.
const FAKE_CHOWN: &str = r#"#!/bin/sh
printf 'chown %s\n' "$*" >> "$FAKE_CALLS"
exit 0
"#;

/// The fake `systemctl`.
///
/// `is-active` answers according to `FAKE_SYSTEMCTL_ACTIVE`:
///
///   * `starting` — the unit is not active until it has been started
///     (the default: the installer has to wait for it),
///   * `already`  — the unit is running, so the installer restarts it,
///   * `never`    — the unit never comes up.
///
/// `FAKE_SYSTEMCTL_FAIL=enable` makes the start fail, the way a broken
/// unit does.
const FAKE_SYSTEMCTL: &str = r#"#!/bin/sh
case "$1" in
  is-active)
    case "$FAKE_SYSTEMCTL_ACTIVE" in
      already) exit 0 ;;
      never) exit 1 ;;
      *)
        if [ -f "$FAKE_CALLS.started" ]; then exit 0; fi
        exit 1
        ;;
    esac
    ;;
  enable)
    printf 'systemctl %s\n' "$*" >> "$FAKE_CALLS"
    : > "$FAKE_CALLS.started"
    if [ "$FAKE_SYSTEMCTL_FAIL" = "enable" ]; then exit 1; fi
    exit 0
    ;;
esac
printf 'systemctl %s\n' "$*" >> "$FAKE_CALLS"
exit 0
"#;

/// The fake `useradd`.
const FAKE_USERADD: &str = r#"#!/bin/sh
printf 'useradd %s\n' "$*" >> "$FAKE_CALLS"
exit 0
"#;

/// The fake `adduser`.
const FAKE_ADDUSER: &str = r#"#!/bin/sh
printf 'adduser %s\n' "$*" >> "$FAKE_CALLS"
exit 0
"#;

/// The fake `getent` — it reports the `pangolin` group as missing
/// unless the test says otherwise, because a release host does not
/// necessarily have it yet.
const FAKE_GETENT: &str = r#"#!/bin/sh
if [ "$1" = "group" ] && [ "$2" = "pangolin" ]; then
  if [ "$FAKE_GROUP_EXISTS" = "1" ]; then
    printf 'pangolin:x:4242:\n'
    exit 0
  fi
  exit 2
fi
exit 2
"#;

/// The sandbox the installer runs in: a private `PATH`, a private
/// install directory and a log of the commands that would have touched
/// the machine.
struct Sandbox {
    dir: TempDir,
    bin: PathBuf,
    calls: PathBuf,
    install_dir: PathBuf,
    systemd_dir: PathBuf,
}

impl Sandbox {
    /// A sandbox with the real tools linked in, the fakes in place and
    /// the digest tool the host has.
    fn new() -> Sandbox {
        Sandbox::with_digest(Some(host_digest_tool()))
    }

    /// A sandbox with exactly one digest tool — `None` leaves the
    /// host without one, which is how the "no digest tool at all" case
    /// is exercised.
    fn with_digest(digest_tool: Option<&str>) -> Sandbox {
        let dir = TempDir::new().expect("temp dir");
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).expect("create the private bin directory");

        for tool in TOOLS {
            if let Some(path) = host_tool(tool) {
                fs::copy(&path, bin.join(tool)).expect("copy the tool");
            }
        }
        if let Some(tool) = digest_tool {
            let path = host_tool(tool)
                .unwrap_or_else(|| panic!("the host has no {tool} — the test cannot use it"));
            fs::copy(&path, bin.join(tool)).expect("copy the digest tool");
        }

        let calls = dir.path().join("calls.log");
        let sandbox = Sandbox {
            install_dir: dir.path().join("install"),
            systemd_dir: dir.path().join("systemd"),
            bin,
            calls,
            dir,
        };
        sandbox.write_fake("uname", FAKE_UNAME);
        sandbox.write_fake("hostname", FAKE_HOSTNAME);
        sandbox.write_fake("id", FAKE_ID);
        sandbox.write_fake("chown", FAKE_CHOWN);
        sandbox.write_fake("systemctl", FAKE_SYSTEMCTL);
        sandbox.write_fake("useradd", FAKE_USERADD);
        sandbox.write_fake("adduser", FAKE_ADDUSER);
        sandbox.write_fake("getent", FAKE_GETENT);
        sandbox
    }

    /// Write one of the fakes into the private `PATH`.
    fn write_fake(&self, name: &str, body: &str) {
        let path = self.bin.join(name);
        fs::write(&path, body).expect("write the fake");
        let mut permissions = fs::metadata(&path).expect("stat the fake").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("make the fake executable");
    }

    /// Remove one of the fakes — the "the host has no systemctl" case.
    fn remove_fake(&self, name: &str) {
        fs::remove_file(self.bin.join(name)).expect("remove the fake");
    }

    /// Run the installer with the sandbox's defaults.
    fn run(&self, base_url: &str, extra: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(host_tool("sh").expect("sh"));
        cmd.arg(repo_root().join("install.sh"))
            .current_dir(self.dir.path())
            .env("PATH", &self.bin)
            .env("HOME", self.dir.path())
            .env("TMPDIR", self.dir.path())
            .env("FAKE_CALLS", &self.calls)
            .env("FAKE_UNAME_S", "Linux")
            .env("FAKE_UNAME_M", "x86_64")
            .env("FAKE_HOSTNAME", "Web01")
            .env("FAKE_ID_UID", "0")
            .env("FAKE_USER_NAME", "root")
            .env("FAKE_USER_UID", "4242")
            .env("FAKE_USER_EXISTS", "1")
            .env("FAKE_GROUP_EXISTS", "0")
            .env("FAKE_SYSTEMCTL_ACTIVE", "starting")
            .env("FAKE_SYSTEMCTL_FAIL", "")
            .env("PANGOLIN_BASE_URL", base_url)
            .env("PANGOLIN_LATEST_URL", format!("{base_url}/releases/latest"))
            .env("PANGOLIN_INSTALL_DIR", &self.install_dir)
            .env("PANGOLIN_SYSTEMD_DIR", &self.systemd_dir)
            // The ambient proxy of a developer machine would swallow
            // the requests to the fake release.
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost");
        for key in [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
        ] {
            cmd.env_remove(key);
        }
        for (key, value) in extra {
            cmd.env(key, value);
        }
        cmd.output().expect("run install.sh")
    }

    /// The commands the fakes recorded, one per line.
    fn calls(&self) -> Vec<String> {
        fs::read_to_string(&self.calls)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

/// The stdout of a run.
fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The stderr of a run.
fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The paths the fake release was asked for.
fn requested_paths(requests: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    requests.lock().expect("the request log").clone()
}

/// The mode of a file, as `ls -l` would print it.
fn mode_of(path: &Path) -> u32 {
    fs::metadata(path)
        .expect("stat the file")
        .permissions()
        .mode()
        & 0o7777
}

/// A fake release: the archive, the units it ships and the checksum
/// file that covers them, served over HTTP.
struct FakeRelease {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
    archive: Vec<u8>,
    /// The units the release ships — empty for a release that predates
    /// the one-line installer.
    units: Vec<(String, Vec<u8>)>,
    /// A checksum file that does not match the archive.
    broken_sums: bool,
}

impl FakeRelease {
    /// A release that ships the archive, both units and a checksum file
    /// that covers all three.
    fn new() -> FakeRelease {
        FakeRelease::build(true, false)
    }

    /// A release that predates the one-line installer: it ships the
    /// archive and the checksum file, but neither the units nor a
    /// packaged configuration.
    fn without_units() -> FakeRelease {
        FakeRelease::build(false, false)
    }

    /// A release whose checksum file records the wrong digest for the
    /// archive.
    fn with_broken_checksum() -> FakeRelease {
        FakeRelease::build(true, true)
    }

    /// Build a release and serve it.
    fn build(with_units: bool, broken_sums: bool) -> FakeRelease {
        let dir = TempDir::new().expect("temp dir");
        let archive = build_archive(&dir.path().join("stage"));
        let mut units = Vec::new();
        if with_units {
            units.push((
                "pangolin-ngx.service".to_string(),
                read_repo_file("deploy/playbooks/roles/ngx/files/ngx.service").into_bytes(),
            ));
            units.push((
                "pangolin-tun.service".to_string(),
                read_repo_file("deploy/playbooks/roles/tun/files/tun.service").into_bytes(),
            ));
        }

        let release = FakeRelease {
            base_url: String::new(),
            requests: Arc::new(Mutex::new(Vec::new())),
            archive,
            units,
            broken_sums,
        };

        let (base_url, requests) = spawn_server(release.handler());
        FakeRelease {
            base_url,
            requests,
            ..release
        }
    }

    /// The request handler of this release.
    ///
    /// The checksum file is built per version, because a release's
    /// checksum file lists the archives of *that* release — and the
    /// installer asks for the one of the version it resolved.
    fn handler(&self) -> Handler {
        let archive = self.archive.clone();
        let units = self.units.clone();
        let broken = self.broken_sums;

        Box::new(move |path: &str| {
            if path == "/releases/latest" {
                return Reply::Redirect(format!("/releases/tag/{RELEASE_VERSION}"));
            }
            if path.starts_with("/releases/tag/") {
                return Reply::Body(Vec::new());
            }

            let mut segments = path.trim_start_matches('/').split('/');
            let version = segments.next().unwrap_or("");
            let name = segments.next().unwrap_or("");
            match name {
                "SHA256SUMS.txt" => {
                    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
                    for target in ["linux_amd64", "linux_arm64", "macos_arm64", "macos_x64"] {
                        let bytes = if broken {
                            b"not the archive".to_vec()
                        } else {
                            archive.clone()
                        };
                        entries.push((format!("pangolin_{version}_{target}.tar.gz"), bytes));
                    }
                    for (unit_name, bytes) in &units {
                        entries.push((unit_name.clone(), bytes.clone()));
                    }
                    let borrowed: Vec<(&str, &[u8])> = entries
                        .iter()
                        .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
                        .collect();
                    Reply::Body(sums_file(&borrowed))
                }
                name if units.iter().any(|(unit, _)| unit == name) => Reply::Body(
                    units
                        .iter()
                        .find(|(unit, _)| unit == name)
                        .map(|(_, bytes)| bytes.clone())
                        .unwrap_or_default(),
                ),
                name if name.starts_with("pangolin_") && name.ends_with(".tar.gz") => {
                    Reply::Body(archive.clone())
                }
                _ => Reply::NotFound,
            }
        })
    }
}

// ── the script itself ───────────────────────────────────────────────────────

/// Remove the POSIX character classes (`[[:space:]]` and friends) from
/// a text.
///
/// They contain `[[` and `]]`, which is also how the bash-only test
/// looks — so the bashism check has to look at the text without them.
fn strip_character_classes(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut rest = source;
    while let Some(at) = rest.find("[[:") {
        out.push_str(&rest[..at]);
        match rest[at..].find(":]]") {
            Some(end) => {
                // The class itself is dropped — it is what the check
                // would trip over.
                rest = &rest[at + end + 3..];
            }
            None => {
                out.push_str(&rest[at..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// The installer must be a POSIX shell script: macOS ships bash 3.2
/// and Linux bash 5.x, and a bashism breaks the installer on one of
/// the two platforms it exists to serve.
#[test]
fn install_script_is_posix_sh() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    let script = repo_root().join("install.sh");
    let source = read_repo_file("install.sh");
    let code = strip_character_classes(&source);

    assert!(
        source.starts_with("#!/bin/sh\n"),
        "install.sh must start with `#!/bin/sh`"
    );
    assert!(
        source.contains("\nset -eu\n"),
        "install.sh must run under `set -eu`"
    );

    let check = Command::new(host_tool("sh").expect("sh"))
        .arg("-n")
        .arg(&script)
        .output()
        .expect("run sh -n");
    assert!(
        check.status.success(),
        "sh -n install.sh failed:\n{}",
        String::from_utf8_lossy(&check.stderr)
    );

    for (pattern, what) in [
        ("[[", "the `[[` test"),
        ("]]", "the `]]` test"),
        (",,", "the `${var,,}` expansion"),
        ("$RANDOM", "the `$RANDOM` variable"),
        ("mapfile", "the `mapfile` builtin"),
        ("readarray", "the `readarray` builtin"),
        ("declare -a", "an array declaration"),
        ("local -a", "an array declaration"),
        ("<(", "a process substitution"),
        (">(", "a process substitution"),
    ] {
        assert!(
            !code.contains(pattern),
            "install.sh must not use {what} (`{pattern}`) — it has to run under POSIX sh"
        );
    }

    // `source` and `.` are bashisms in disguise — POSIX spells it
    // `.`.  Neither may start a line.
    for (number, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        assert!(
            !trimmed.starts_with("source ") && !trimmed.starts_with(". "),
            "install.sh:{} sources another file — POSIX sh has no `source`",
            number + 1
        );
    }
}

/// The installer has to be executable — it is published as a release
/// asset and the one-liner pipes it into `sh`, but a checkout that
/// lost the bit would break `curl … | sudo sh` for everybody who
/// clones the repository.
#[test]
fn install_script_is_executable() {
    let script = repo_root().join("install.sh");
    let mode = mode_of(&script);
    assert_eq!(
        mode, 0o755,
        "install.sh must be committed with mode 100755, got {mode:o}"
    );

    // The git index is what the release copies the file from.
    if let Some(git) = host_tool("git") {
        let output = Command::new(git)
            .arg("-C")
            .arg(repo_root())
            .args(["ls-files", "-s", "install.sh"])
            .output()
            .expect("run git ls-files");
        let listed = String::from_utf8_lossy(&output.stdout);
        assert!(
            listed.starts_with("100755 "),
            "install.sh must be tracked with mode 100755, git says: {listed}"
        );
    }
}

// ── the platform table ──────────────────────────────────────────────────────

/// The installer has to pick the archive that matches the machine —
/// and refuse the ones it cannot serve.
#[test]
fn install_script_picks_the_archive_for_the_platform() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    for (os, machine, expected) in [
        ("Linux", "x86_64", "pangolin_v1.2.3_linux_amd64.tar.gz"),
        ("Linux", "amd64", "pangolin_v1.2.3_linux_amd64.tar.gz"),
        ("Linux", "aarch64", "pangolin_v1.2.3_linux_arm64.tar.gz"),
        ("Linux", "arm64", "pangolin_v1.2.3_linux_arm64.tar.gz"),
        ("Darwin", "arm64", "pangolin_v1.2.3_macos_arm64.tar.gz"),
        ("Darwin", "aarch64", "pangolin_v1.2.3_macos_arm64.tar.gz"),
        ("Darwin", "x86_64", "pangolin_v1.2.3_macos_x64.tar.gz"),
        ("Darwin", "amd64", "pangolin_v1.2.3_macos_x64.tar.gz"),
    ] {
        let release = FakeRelease::new();
        let sandbox = Sandbox::new();
        let output = sandbox.run(
            &release.base_url,
            &[("FAKE_UNAME_S", os), ("FAKE_UNAME_M", machine)],
        );
        assert!(
            output.status.success(),
            "the install on {os}/{machine} failed:\n{}{}",
            stdout_of(&output),
            stderr_of(&output)
        );

        let requested = requested_paths(&release.requests);
        assert!(
            requested.iter().any(|path| path.ends_with(expected)),
            "on {os}/{machine} the installer must ask for {expected}, it asked for {requested:?}"
        );
    }
}

/// A platform the release does not publish has to be refused, with the
/// supported ones named — a silent fallback would install the wrong
/// binary.
#[test]
fn install_script_refuses_an_unsupported_platform() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    let release = FakeRelease::new();
    let sandbox = Sandbox::new();
    let output = sandbox.run(
        &release.base_url,
        &[("FAKE_UNAME_S", "Linux"), ("FAKE_UNAME_M", "riscv64")],
    );

    assert!(
        !output.status.success(),
        "an unsupported platform must fail the install:\n{}",
        stdout_of(&output)
    );
    let stderr = stderr_of(&output);
    for platform in ["linux_amd64", "linux_arm64", "macos_arm64", "macos_x64"] {
        assert!(
            stderr.contains(platform),
            "the error must name the supported platform {platform}:\n{stderr}"
        );
    }
    assert!(
        !sandbox.install_dir.exists(),
        "a refused install must not create the install directory"
    );
}

// ── the version ─────────────────────────────────────────────────────────────

/// Without `PANGOLIN_VERSION` the installer follows the redirect of
/// `releases/latest` — the tag is the version, and no API call is
/// needed.
#[test]
fn install_script_resolves_the_latest_release() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    let release = FakeRelease::new();
    let sandbox = Sandbox::new();
    let output = sandbox.run(&release.base_url, &[]);
    assert!(
        output.status.success(),
        "the install failed:\n{}{}",
        stdout_of(&output),
        stderr_of(&output)
    );

    let requested = requested_paths(&release.requests);
    assert!(
        requested
            .iter()
            .any(|path| path.ends_with(&format!("pangolin_{RELEASE_VERSION}_linux_amd64.tar.gz"))),
        "the installer must ask for the latest release's archive, it asked for {requested:?}"
    );
    assert!(
        requested.iter().any(|path| path == "/releases/latest"),
        "the installer must resolve the version through releases/latest, it asked for {requested:?}"
    );
}

/// `PANGOLIN_VERSION` pins the version — with or without the leading
/// `v`, because both spellings are what people type.
#[test]
fn install_script_honours_pinned_versions() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    for pinned in ["0.3.0", "v0.3.0"] {
        let release = FakeRelease::new();
        let sandbox = Sandbox::new();
        let output = sandbox.run(&release.base_url, &[("PANGOLIN_VERSION", pinned)]);
        assert!(
            output.status.success(),
            "the install of {pinned} failed:\n{}{}",
            stdout_of(&output),
            stderr_of(&output)
        );

        let requested = requested_paths(&release.requests);
        assert!(
            requested
                .iter()
                .any(|path| path.contains("/v0.3.0/") && path.ends_with(".tar.gz")),
            "PANGOLIN_VERSION={pinned} must ask for the v0.3.0 archive, it asked for {requested:?}"
        );
        assert!(
            !requested.iter().any(|path| path == "/releases/latest"),
            "a pinned version must not resolve the latest release, it asked for {requested:?}"
        );
    }
}

// ── the environment ─────────────────────────────────────────────────────────

/// Every knob of the installer is an environment variable, and every
/// one of them changes what it does.
#[test]
fn install_script_honours_the_environment() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    // A different install directory, and the units have to follow it.
    let release = FakeRelease::new();
    let sandbox = Sandbox::new();
    let install_dir = sandbox.dir.path().join("elsewhere");
    let output = sandbox.run(
        &release.base_url,
        &[
            ("PANGOLIN_INSTALL_DIR", install_dir.to_str().unwrap()),
            ("PANGOLIN_VERSION", "v1.2.3"),
        ],
    );
    assert!(
        output.status.success(),
        "the install failed:\n{}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        install_dir.join("pangolin-ngx").is_file(),
        "the installer must honour PANGOLIN_INSTALL_DIR"
    );
    assert!(
        !sandbox.install_dir.exists(),
        "the installer must not write to the default install directory"
    );

    // PANGOLIN_NO_SERVICE skips the whole systemd half.
    let release = FakeRelease::new();
    let sandbox = Sandbox::new();
    let output = sandbox.run(
        &release.base_url,
        &[("PANGOLIN_NO_SERVICE", "1"), ("PANGOLIN_VERSION", "v1.2.3")],
    );
    assert!(
        output.status.success(),
        "the install failed:\n{}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let calls = sandbox.calls();
    assert!(
        !calls.iter().any(|call| call.starts_with("systemctl")),
        "PANGOLIN_NO_SERVICE=1 must not call systemctl, it called {calls:?}"
    );
    assert!(
        !sandbox.systemd_dir.exists(),
        "PANGOLIN_NO_SERVICE=1 must not install a unit"
    );
}

// ── the checksums ───────────────────────────────────────────────────────────

/// The archive is verified before anything is written — a corrupt
/// download must not leave a half-installed gateway behind.
#[test]
fn install_script_verifies_before_it_installs() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    let release = FakeRelease::with_broken_checksum();
    let sandbox = Sandbox::new();
    let output = sandbox.run(&release.base_url, &[("PANGOLIN_VERSION", "v1.2.3")]);

    assert!(
        !output.status.success(),
        "a checksum mismatch must fail the install:\n{}",
        stdout_of(&output)
    );
    assert!(
        stderr_of(&output).contains("pangolin_v1.2.3_linux_amd64.tar.gz"),
        "the error must name the file that failed:\n{}",
        stderr_of(&output)
    );
    assert!(
        !sandbox.install_dir.exists(),
        "a failed verification must not leave anything in the install directory"
    );
}

/// `SHA256SUMS.txt` lists every archive of the release, but only the
/// one this machine downloaded can be checked — and the entry that is
/// missing must be reported, not treated as a pass.
#[test]
fn install_script_checks_only_what_it_downloaded() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    // A release whose checksum file holds the three entries this
    // machine needs and nothing else — the archive of the *other*
    // platforms is not there, and the installer must not trip over
    // its absence.
    let release = FakeRelease::new();
    let sandbox = Sandbox::new();
    let output = sandbox.run(&release.base_url, &[("PANGOLIN_VERSION", "v1.2.3")]);
    assert!(
        output.status.success(),
        "the install failed:\n{}{}",
        stdout_of(&output),
        stderr_of(&output)
    );

    // Now the same release without the archive's entry.
    let release = FakeRelease::new();
    let (base_url, _requests) = spawn_server({
        let archive = release.archive.clone();
        let ngx_unit = release.units[0].1.clone();
        let tun_unit = release.units[1].1.clone();
        // The checksum file covers the units, but not the archive.
        let sums = sums_file(&[
            ("pangolin-ngx.service", &ngx_unit),
            ("pangolin-tun.service", &tun_unit),
        ]);
        Box::new(
            move |path: &str| match path.rsplit('/').next().unwrap_or("") {
                "SHA256SUMS.txt" => Reply::Body(sums.clone()),
                "pangolin-ngx.service" => Reply::Body(ngx_unit.clone()),
                "pangolin-tun.service" => Reply::Body(tun_unit.clone()),
                name if name.starts_with("pangolin_") && name.ends_with(".tar.gz") => {
                    Reply::Body(archive.clone())
                }
                _ => Reply::NotFound,
            },
        )
    });

    let sandbox = Sandbox::new();
    let output = sandbox.run(&base_url, &[("PANGOLIN_VERSION", "v1.2.3")]);
    assert!(
        !output.status.success(),
        "a missing checksum entry must fail the install:\n{}",
        stdout_of(&output)
    );
    assert!(
        stderr_of(&output).contains("pangolin_v1.2.3_linux_amd64.tar.gz"),
        "the error must name the missing entry:\n{}",
        stderr_of(&output)
    );
}

/// A release that predates the one-line installer ships neither the
/// systemd units nor a packaged configuration.  The installer has to
/// say so — a 404 on an asset the operator never heard of is not an
/// explanation.
#[test]
fn install_script_explains_a_release_without_the_units() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    let release = FakeRelease::without_units();
    let sandbox = Sandbox::new();
    let output = sandbox.run(&release.base_url, &[("PANGOLIN_VERSION", "v0.0.2")]);

    assert!(
        !output.status.success(),
        "a release without the units must fail the install:\n{}",
        stdout_of(&output)
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("pangolin-ngx.service"),
        "the error must name the missing asset:\n{stderr}"
    );
    assert!(
        stderr.contains("predates"),
        "the error must say that the release is too old:\n{stderr}"
    );
    assert!(
        stderr.contains("getting-started"),
        "the error must point at the manual installation:\n{stderr}"
    );
    assert!(
        !sandbox.install_dir.exists(),
        "a refused install must not create the install directory"
    );
}

/// An archive without a packaged configuration cannot be configured by
/// the installer either — the gateway would run on its built-in
/// defaults (the development ports and the `admin` password).
#[test]
fn install_script_explains_a_release_without_a_configuration() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    // An archive with the two binaries only — the layout of the
    // releases before the configuration was packaged.
    let dir = TempDir::new().expect("temp dir");
    let stage = dir.path().join("stage");
    fs::create_dir_all(&stage).expect("create the staging directory");
    fs::write(stage.join("pangolin-ngx"), "#!/bin/sh\n").expect("write the fake gateway");
    fs::write(stage.join("pangolin-tun"), "#!/bin/sh\n").expect("write the fake tun");
    let archive_path = dir.path().join("archive.tar.gz");
    let status = Command::new(host_tool("tar").expect("tar"))
        .arg("-C")
        .arg(&stage)
        .arg("-czf")
        .arg(&archive_path)
        .arg("pangolin-ngx")
        .arg("pangolin-tun")
        .status()
        .expect("run tar");
    assert!(status.success(), "tar failed to build the test archive");
    let archive = fs::read(&archive_path).expect("read the test archive");

    let release = FakeRelease::new();
    let (base_url, _requests) = spawn_server({
        let archive = archive.clone();
        let ngx_unit = release.units[0].1.clone();
        let tun_unit = release.units[1].1.clone();
        let sums = sums_file(&[
            ("pangolin_v1.2.3_linux_amd64.tar.gz", &archive),
            ("pangolin-ngx.service", &ngx_unit),
            ("pangolin-tun.service", &tun_unit),
        ]);
        Box::new(
            move |path: &str| match path.rsplit('/').next().unwrap_or("") {
                "SHA256SUMS.txt" => Reply::Body(sums.clone()),
                "pangolin-ngx.service" => Reply::Body(ngx_unit.clone()),
                "pangolin-tun.service" => Reply::Body(tun_unit.clone()),
                name if name.starts_with("pangolin_") && name.ends_with(".tar.gz") => {
                    Reply::Body(archive.clone())
                }
                _ => Reply::NotFound,
            },
        )
    });

    let sandbox = Sandbox::new();
    let output = sandbox.run(&base_url, &[("PANGOLIN_VERSION", "v1.2.3")]);

    assert!(
        !output.status.success(),
        "an archive without a configuration must fail the install:\n{}",
        stdout_of(&output)
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("ngx.yml"),
        "the error must name the missing configuration:\n{stderr}"
    );
    assert!(
        stderr.contains("predates"),
        "the error must say that the release is too old:\n{stderr}"
    );
}

/// The digest tool is the first of `sha256sum`, `shasum -a 256` and
/// `openssl dgst -sha256` that the host has — macOS has no
/// `sha256sum`, only `shasum`.
#[test]
fn install_script_falls_back_to_another_digest_tool() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    // Without `sha256sum` the install still has to verify the archive.
    if host_tool("shasum").is_some() {
        let release = FakeRelease::new();
        let sandbox = Sandbox::with_digest(Some("shasum"));
        let output = sandbox.run(&release.base_url, &[("PANGOLIN_VERSION", "v1.2.3")]);
        assert!(
            output.status.success(),
            "the install must work without sha256sum:\n{}{}",
            stdout_of(&output),
            stderr_of(&output)
        );
    }

    // The last resort: a host with openssl and neither of the other
    // two — the shape of a minimal container image.
    if host_tool("openssl").is_some() {
        let release = FakeRelease::new();
        let sandbox = Sandbox::with_digest(Some("openssl"));
        let output = sandbox.run(&release.base_url, &[("PANGOLIN_VERSION", "v1.2.3")]);
        assert!(
            output.status.success(),
            "the install must work with openssl alone:\n{}{}",
            stdout_of(&output),
            stderr_of(&output)
        );
    }

    // Without any of the three it has to say so.
    let release = FakeRelease::new();
    let sandbox = Sandbox::with_digest(None);
    let output = sandbox.run(&release.base_url, &[("PANGOLIN_VERSION", "v1.2.3")]);
    assert!(
        !output.status.success(),
        "a host without a digest tool must fail the install:\n{}",
        stdout_of(&output)
    );
    let stderr = stderr_of(&output);
    for tool in ["sha256sum", "shasum", "openssl"] {
        assert!(
            stderr.contains(tool),
            "the error must name {tool} as an option:\n{stderr}"
        );
    }
}

/// The version ends up in a URL and in a file name, so it must not be
/// able to escape either.
#[test]
fn install_script_rejects_a_malicious_version() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    let release = FakeRelease::new();
    let sandbox = Sandbox::new();
    for version in ["v1.2.3/../../etc", "v1.2.3;rm -rf /", "../v1.2.3"] {
        let output = sandbox.run(&release.base_url, &[("PANGOLIN_VERSION", version)]);
        assert!(
            !output.status.success(),
            "the version `{version}` must be refused:\n{}",
            stdout_of(&output)
        );
        assert!(
            stderr_of(&output).contains("invalid version"),
            "the error must say that the version is invalid:\n{}",
            stderr_of(&output)
        );
    }
}

// ── the installed files ─────────────────────────────────────────────────────

/// The binaries are executable, the configuration files are not
/// readable by anybody else — they carry the admin password and the
/// tun's bearer token.
#[test]
fn install_script_sets_the_installed_permissions() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    let release = FakeRelease::new();
    let sandbox = Sandbox::new();
    let output = sandbox.run(&release.base_url, &[("PANGOLIN_VERSION", "v1.2.3")]);
    assert!(
        output.status.success(),
        "the install failed:\n{}{}",
        stdout_of(&output),
        stderr_of(&output)
    );

    for name in ["pangolin-ngx", "pangolin-tun"] {
        assert_eq!(
            mode_of(&sandbox.install_dir.join(name)),
            0o755,
            "{name} must be installed 0755"
        );
    }
    for name in ["ngx.yml", "tun.yml"] {
        assert_eq!(
            mode_of(&sandbox.install_dir.join(name)),
            0o600,
            "{name} must be installed 0600 — it carries a secret"
        );
    }
}

/// The configuration refers to `./certs`, `./logs/bots` and
/// `./logs/referers` — relative to the unit's working directory, so
/// the directories have to exist before the gateway starts.
#[test]
fn install_script_creates_the_runtime_directories() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    let release = FakeRelease::new();
    let sandbox = Sandbox::new();
    let output = sandbox.run(&release.base_url, &[("PANGOLIN_VERSION", "v1.2.3")]);
    assert!(
        output.status.success(),
        "the install failed:\n{}{}",
        stdout_of(&output),
        stderr_of(&output)
    );

    for directory in ["certs", "logs/bots", "logs/referers"] {
        assert!(
            sandbox.install_dir.join(directory).is_dir(),
            "the installer must create {directory}"
        );
    }
}

/// The install directory belongs to the user the unit runs as — but
/// only if that user exists.
#[test]
fn install_script_hands_the_directory_over_only_when_the_user_exists() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    let release = FakeRelease::new();
    let sandbox = Sandbox::new();
    let output = sandbox.run(
        &release.base_url,
        &[("PANGOLIN_VERSION", "v1.2.3"), ("FAKE_USER_EXISTS", "1")],
    );
    assert!(
        output.status.success(),
        "the install failed:\n{}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let calls = sandbox.calls();
    assert!(
        calls
            .iter()
            .any(|call| call.contains("chown -R pangolin:pangolin")),
        "the installer must hand the directory over to the pangolin user, it called {calls:?}"
    );

    // Without the user there is nobody to hand it over to.
    let release = FakeRelease::new();
    let sandbox = Sandbox::new();
    let output = sandbox.run(
        &release.base_url,
        &[("PANGOLIN_VERSION", "v1.2.3"), ("FAKE_USER_EXISTS", "0")],
    );
    assert!(
        output.status.success(),
        "the install failed:\n{}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let calls = sandbox.calls();
    assert!(
        !calls.iter().any(|call| call.starts_with("chown")),
        "without the pangolin user the installer must not chown, it called {calls:?}"
    );
}

// ── the configuration rewrite ───────────────────────────────────────────────

/// The shipped configuration is a development configuration.  Five
/// settings have to change before the gateway faces the internet — and
/// nothing else may.
#[test]
fn install_script_rewrites_the_five_production_settings() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    let release = FakeRelease::new();
    let sandbox = Sandbox::new();
    let output = sandbox.run(&release.base_url, &[("PANGOLIN_VERSION", "v1.2.3")]);
    assert!(
        output.status.success(),
        "the install failed:\n{}{}",
        stdout_of(&output),
        stderr_of(&output)
    );

    let installed = fs::read_to_string(sandbox.install_dir.join("ngx.yml")).expect("read ngx.yml");
    let original = read_repo_file("ngx.yml");

    for expected in [
        "  http: 0.0.0.0:80 ",
        "  https: 0.0.0.0:443 ",
        "  password: pangolin ",
        "  email: \"pangolin@web01.local\" ",
        "  level: info ",
    ] {
        assert!(
            installed.contains(expected),
            "the installed ngx.yml must contain `{expected}`:\n{installed}"
        );
    }

    // The admin interface keeps the address the release shipped — the
    // installer warns about it instead of silently narrowing it.
    assert!(
        installed.contains("  addr: 0.0.0.0:9081 "),
        "the admin interface must keep its address:\n{installed}"
    );
    assert!(
        installed.contains("  username: admin"),
        "the admin user name must not change:\n{installed}"
    );

    // Every other line is byte for byte the one the release shipped.
    let original_lines: Vec<&str> = original.lines().collect();
    let installed_lines: Vec<&str> = installed.lines().collect();
    assert_eq!(
        original_lines.len(),
        installed_lines.len(),
        "the rewrite must not add or drop a line"
    );
    let changed: Vec<&str> = original_lines
        .iter()
        .zip(installed_lines.iter())
        .filter(|(before, after)| before != after)
        .map(|(before, _)| *before)
        .collect();
    assert_eq!(
        changed.len(),
        5,
        "the rewrite must touch exactly five lines, it touched {changed:?}"
    );
}

/// The ACME contact address is derived from the host name, because the
/// installer cannot know the operator's mailbox.
#[test]
fn install_script_derives_the_acme_email_from_the_host_name() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    for (host, expected) in [
        ("Web01", "pangolin@web01.local"),
        ("gw-01.example", "pangolin@gw-01-example.local"),
        ("", "pangolin@localhost.local"),
        ("...", "pangolin@localhost.local"),
    ] {
        let release = FakeRelease::new();
        let sandbox = Sandbox::new();
        let output = sandbox.run(
            &release.base_url,
            &[("PANGOLIN_VERSION", "v1.2.3"), ("FAKE_HOSTNAME", host)],
        );
        assert!(
            output.status.success(),
            "the install failed:\n{}{}",
            stdout_of(&output),
            stderr_of(&output)
        );

        let installed =
            fs::read_to_string(sandbox.install_dir.join("ngx.yml")).expect("read ngx.yml");
        assert!(
            installed.contains(&format!("  email: \"{expected}\" ")),
            "a host name of `{host}` must produce {expected}:\n{installed}"
        );
    }
}

/// `tun.yml` carries the tunnel's connection target, its token and
/// its name — the installer knows none of them, so the file must
/// survive the install byte for byte.
#[test]
fn install_script_never_touches_tun_yml() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    let release = FakeRelease::new();
    let sandbox = Sandbox::new();
    let output = sandbox.run(&release.base_url, &[("PANGOLIN_VERSION", "v1.2.3")]);
    assert!(
        output.status.success(),
        "the install failed:\n{}{}",
        stdout_of(&output),
        stderr_of(&output)
    );

    let installed = fs::read(sandbox.install_dir.join("tun.yml")).expect("read tun.yml");
    assert_eq!(
        installed,
        read_repo_file("tun.yml").into_bytes(),
        "tun.yml must be installed byte for byte"
    );
}

// ── the service ─────────────────────────────────────────────────────────────

/// The gateway is registered and started; the tunnel node is only
/// installed.  A tunnel node whose `(name, token)` row does not exist
/// yet is rejected at the handshake, so starting it would only produce
/// a restart loop.
#[test]
fn install_script_starts_the_gateway_and_not_the_tunnel_node() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    let release = FakeRelease::new();
    let sandbox = Sandbox::new();
    let output = sandbox.run(&release.base_url, &[("PANGOLIN_VERSION", "v1.2.3")]);
    assert!(
        output.status.success(),
        "the install failed:\n{}{}",
        stdout_of(&output),
        stderr_of(&output)
    );

    let calls = sandbox.calls();
    assert!(
        calls.iter().any(|call| call == "systemctl daemon-reload"),
        "the installer must reload systemd, it called {calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|call| call == "systemctl enable --now pangolin-ngx"),
        "the installer must enable and start the gateway, it called {calls:?}"
    );
    assert!(
        !calls.iter().any(|call| call.contains("pangolin-tun")),
        "the installer must not start the tunnel node, it called {calls:?}"
    );

    // Both units are installed — the operator should not have to go
    // looking for the file when they register a tunnel node later.
    for unit in ["pangolin-ngx.service", "pangolin-tun.service"] {
        let path = sandbox.systemd_dir.join(unit);
        assert!(path.is_file(), "{unit} must be installed");
        assert_eq!(mode_of(&path), 0o644, "{unit} must be installed 0644");
    }
}

/// An existing `pangolin` user is kept — recreating it would change
/// the ownership of every file the operator already has.
#[test]
fn install_script_keeps_an_existing_pangolin_user() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    let release = FakeRelease::new();
    let sandbox = Sandbox::new();
    let output = sandbox.run(
        &release.base_url,
        &[("PANGOLIN_VERSION", "v1.2.3"), ("FAKE_USER_EXISTS", "1")],
    );
    assert!(
        output.status.success(),
        "the install failed:\n{}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let calls = sandbox.calls();
    assert!(
        !calls.iter().any(|call| call.starts_with("useradd")),
        "an existing user must not be recreated, it called {calls:?}"
    );
    assert!(
        !calls.iter().any(|call| call.starts_with("adduser")),
        "an existing user must not be recreated, it called {calls:?}"
    );

    // Without the user it is created — with the arguments that make it
    // a system account that cannot log in.
    let release = FakeRelease::new();
    let sandbox = Sandbox::new();
    let output = sandbox.run(
        &release.base_url,
        &[("PANGOLIN_VERSION", "v1.2.3"), ("FAKE_USER_EXISTS", "0")],
    );
    assert!(
        output.status.success(),
        "the install failed:\n{}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let calls = sandbox.calls();
    assert!(
        calls.iter().any(|call| {
            call.starts_with("useradd")
                && call.contains("--system")
                && call.contains("--no-create-home")
                && call.contains("/usr/sbin/nologin")
                && call.ends_with("pangolin")
        }),
        "the installer must create the system user, it called {calls:?}"
    );
}

/// The user needs a group of its own: the unit says `Group=pangolin`
/// and the install directory is handed over to `pangolin:pangolin`.
/// `useradd` only creates a same-named group where USERGROUPS_ENAB is
/// yes — on the RHEL family it is not, and the user would land in the
/// default group instead.
#[test]
fn install_script_creates_the_pangolin_group() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    let release = FakeRelease::new();
    let sandbox = Sandbox::new();
    let output = sandbox.run(
        &release.base_url,
        &[("PANGOLIN_VERSION", "v1.2.3"), ("FAKE_USER_EXISTS", "0")],
    );
    assert!(
        output.status.success(),
        "the install failed:\n{}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let calls = sandbox.calls();
    assert!(
        calls
            .iter()
            .any(|call| call.starts_with("useradd") && call.contains("--user-group")),
        "the installer must ask for the group, it called {calls:?}"
    );
}

/// A unit the operator edited is kept as `<name>.bak`, the same
/// rolling-backup convention the binaries get.
#[test]
fn install_script_keeps_a_previous_unit() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    let release = FakeRelease::new();
    let sandbox = Sandbox::new();
    let output = sandbox.run(&release.base_url, &[("PANGOLIN_VERSION", "v1.2.3")]);
    assert!(
        output.status.success(),
        "the first install failed:\n{}{}",
        stdout_of(&output),
        stderr_of(&output)
    );

    // The operator edits the unit.
    let unit = sandbox.systemd_dir.join("pangolin-ngx.service");
    fs::write(&unit, "# edited by the operator\n").expect("edit the unit");

    let output = sandbox.run(&release.base_url, &[("PANGOLIN_VERSION", "v1.2.3")]);
    assert!(
        output.status.success(),
        "the second install failed:\n{}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        sandbox
            .systemd_dir
            .join("pangolin-ngx.service.bak")
            .is_file(),
        "the previous unit must be kept as pangolin-ngx.service.bak"
    );
    assert!(
        fs::read_to_string(&unit)
            .expect("read the installed unit")
            .contains("ExecStart="),
        "the installed unit must be the release's own file"
    );
}

/// `enable --now` leaves a running unit alone, so the freshly
/// installed binary would not be the one serving.
#[test]
fn install_script_restarts_a_running_gateway() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    let release = FakeRelease::new();
    let sandbox = Sandbox::new();
    let output = sandbox.run(
        &release.base_url,
        &[
            ("PANGOLIN_VERSION", "v1.2.3"),
            ("FAKE_SYSTEMCTL_ACTIVE", "already"),
        ],
    );
    assert!(
        output.status.success(),
        "the install failed:\n{}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let calls = sandbox.calls();
    assert!(
        calls
            .iter()
            .any(|call| call == "systemctl restart pangolin-ngx"),
        "a gateway that was already running must be restarted, it called {calls:?}"
    );
}

/// A gateway that does not come up is a failed install — but the
/// operator still needs the credentials, the ports and the tunnel
/// steps, so the summary has to be printed before the script exits.
#[test]
fn install_script_still_summarizes_a_gateway_that_did_not_start() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    let release = FakeRelease::new();
    let sandbox = Sandbox::new();
    let output = sandbox.run(
        &release.base_url,
        &[
            ("PANGOLIN_VERSION", "v1.2.3"),
            ("FAKE_SYSTEMCTL_FAIL", "enable"),
        ],
    );

    assert!(
        !output.status.success(),
        "a gateway that did not start must be reported as a failure:\n{}",
        stdout_of(&output)
    );
    let printed = stdout_of(&output);
    for expected in ["9081", "9001", "pangolin-tun", "pangolin.db"] {
        assert!(
            printed.contains(expected),
            "the summary must still mention {expected}:\n{printed}"
        );
    }
    assert!(
        stderr_of(&output).contains("systemctl status pangolin-ngx"),
        "the failure must say where to look:\n{}",
        stderr_of(&output)
    );
}

/// The install directory ends up in the unit's `WorkingDirectory=` and
/// `ExecStart=`, and systemd resolves a relative path against `/`.
#[test]
fn install_script_requires_an_absolute_install_dir() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    let release = FakeRelease::new();
    let sandbox = Sandbox::new();
    let output = sandbox.run(
        &release.base_url,
        &[
            ("PANGOLIN_VERSION", "v1.2.3"),
            ("PANGOLIN_INSTALL_DIR", "relative/pangolin"),
        ],
    );

    assert!(
        !output.status.success(),
        "a relative install directory must be refused:\n{}",
        stdout_of(&output)
    );
    assert!(
        stderr_of(&output).contains("absolute path"),
        "the error must say that the path has to be absolute:\n{}",
        stderr_of(&output)
    );
}

/// The unit ships with the default install directory baked in.  A
/// different install directory has to be reflected in it, or systemd
/// would start a binary that is not there.
#[test]
fn install_script_rewrites_the_unit_paths() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    let release = FakeRelease::new();
    let sandbox = Sandbox::new();
    let install_dir = sandbox.dir.path().join("opt/pangolin");
    let output = sandbox.run(
        &release.base_url,
        &[
            ("PANGOLIN_VERSION", "v1.2.3"),
            ("PANGOLIN_INSTALL_DIR", install_dir.to_str().unwrap()),
        ],
    );
    assert!(
        output.status.success(),
        "the install failed:\n{}{}",
        stdout_of(&output),
        stderr_of(&output)
    );

    let unit = fs::read_to_string(sandbox.systemd_dir.join("pangolin-ngx.service"))
        .expect("read the installed unit");
    assert!(
        !unit.contains(DEFAULT_INSTALL_DIR),
        "the unit must not point at the default install directory:\n{unit}"
    );
    assert!(
        unit.contains(&format!(
            "ExecStart={}/pangolin-ngx --config {}/ngx.yml",
            install_dir.display(),
            install_dir.display()
        )),
        "the unit must start the gateway of the chosen install directory:\n{unit}"
    );
    assert!(
        unit.contains(&format!("WorkingDirectory={}", install_dir.display())),
        "the unit must work in the chosen install directory:\n{unit}"
    );
}

/// macOS has no systemd — the installer must install the files and
/// print how to start the gateway, and nothing else.
#[test]
fn install_script_skips_the_service_on_macos() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    let release = FakeRelease::new();
    let sandbox = Sandbox::new();
    let output = sandbox.run(
        &release.base_url,
        &[
            ("PANGOLIN_VERSION", "v1.2.3"),
            ("FAKE_UNAME_S", "Darwin"),
            ("FAKE_UNAME_M", "arm64"),
        ],
    );
    assert!(
        output.status.success(),
        "the install failed:\n{}{}",
        stdout_of(&output),
        stderr_of(&output)
    );

    let calls = sandbox.calls();
    assert!(
        !calls.iter().any(|call| call.starts_with("systemctl")),
        "macOS has no systemd, the installer called {calls:?}"
    );
    assert!(
        !sandbox.systemd_dir.exists(),
        "macOS has no systemd, the installer must not write a unit"
    );
    assert!(
        stdout_of(&output).contains("pangolin-ngx"),
        "the installer must print how to start the gateway:\n{}",
        stdout_of(&output)
    );
}

/// A host without systemd — a container, most of the time — gets the
/// files and a warning, not a failed install.
#[test]
fn install_script_skips_the_service_without_systemctl() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    let release = FakeRelease::new();
    let sandbox = Sandbox::new();
    sandbox.remove_fake("systemctl");
    let output = sandbox.run(&release.base_url, &[("PANGOLIN_VERSION", "v1.2.3")]);

    assert!(
        output.status.success(),
        "a host without systemd must still get the files:\n{}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let combined = format!("{}{}", stdout_of(&output), stderr_of(&output));
    assert!(
        combined.to_lowercase().contains("systemctl"),
        "the installer must say that the service was not registered:\n{combined}"
    );
}

// ── re-running the installer ────────────────────────────────────────────────

/// A second run must not throw away the operator's configuration, and
/// must keep the previous binary around.
#[test]
fn install_script_is_idempotent() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    let release = FakeRelease::new();
    let sandbox = Sandbox::new();
    let output = sandbox.run(&release.base_url, &[("PANGOLIN_VERSION", "v1.2.3")]);
    assert!(
        output.status.success(),
        "the first install failed:\n{}{}",
        stdout_of(&output),
        stderr_of(&output)
    );

    // The operator edits the configuration.
    let ngx_yml = sandbox.install_dir.join("ngx.yml");
    let edited = format!(
        "{}\n# the operator was here\n",
        fs::read_to_string(&ngx_yml).unwrap()
    );
    fs::write(&ngx_yml, &edited).expect("edit the configuration");

    let output = sandbox.run(&release.base_url, &[("PANGOLIN_VERSION", "v1.2.3")]);
    assert!(
        output.status.success(),
        "the second install failed:\n{}{}",
        stdout_of(&output),
        stderr_of(&output)
    );

    assert_eq!(
        fs::read_to_string(&ngx_yml).unwrap(),
        edited,
        "an existing ngx.yml must not be overwritten"
    );
    assert!(
        sandbox.install_dir.join("pangolin-ngx.bak").is_file(),
        "the previous binary must be kept as pangolin-ngx.bak"
    );
}

// ── what the operator is told ───────────────────────────────────────────────

/// The admin interface ships wide open — the installer says so before
/// it does anything and again in the summary.
#[test]
fn install_script_warns_about_the_admin_interface() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    let release = FakeRelease::new();
    let sandbox = Sandbox::new();
    let output = sandbox.run(&release.base_url, &[("PANGOLIN_VERSION", "v1.2.3")]);
    assert!(
        output.status.success(),
        "the install failed:\n{}{}",
        stdout_of(&output),
        stderr_of(&output)
    );

    let combined = format!("{}{}", stdout_of(&output), stderr_of(&output));
    assert!(
        combined.contains("0.0.0.0:9081"),
        "the warning must name the admin address:\n{combined}"
    );
    assert!(
        combined.contains("pangolin"),
        "the warning must name the password:\n{combined}"
    );
    assert!(
        combined.to_lowercase().contains("plain http"),
        "the warning must say that the admin interface speaks plain HTTP:\n{combined}"
    );

    // The warning has to be out before the first download.
    let warning_at = combined.find("0.0.0.0:9081").expect("the warning");
    let download_at = combined.find("Downloading").expect("the downloads");
    assert!(
        warning_at < download_at,
        "the warning must come before the first download:\n{combined}"
    );
}

/// The summary is the operator's checklist: where the gateway is, how
/// to reach it, which ports to open, and what is still missing.
#[test]
fn install_script_prints_the_closing_summary() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    let release = FakeRelease::new();
    let sandbox = Sandbox::new();
    let output = sandbox.run(&release.base_url, &[("PANGOLIN_VERSION", "v1.2.3")]);
    assert!(
        output.status.success(),
        "the install failed:\n{}{}",
        stdout_of(&output),
        stderr_of(&output)
    );

    let printed = stdout_of(&output);
    for expected in [
        "9081",
        "9001",
        "pangolin.db",
        "pangolin-tun",
        "80",
        "443",
        "pangolin",
    ] {
        assert!(
            printed.contains(expected),
            "the summary must mention {expected}:\n{printed}"
        );
    }
    assert!(
        printed.contains(&sandbox.install_dir.to_string_lossy().into_owned()),
        "the summary must name the install directory:\n{printed}"
    );
    assert!(
        printed.contains(RELEASE_VERSION),
        "the summary must name the installed version:\n{printed}"
    );
}

/// The steps that need root have to say so — an operator who forgot
/// `sudo` gets a message that tells them what to do.
#[test]
fn install_script_asks_for_sudo_when_it_is_not_root() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    let release = FakeRelease::new();
    let sandbox = Sandbox::new();
    let output = sandbox.run(
        &release.base_url,
        &[("PANGOLIN_VERSION", "v1.2.3"), ("FAKE_ID_UID", "1000")],
    );

    assert!(
        !output.status.success(),
        "an install that needs root must fail instead of half-working:\n{}",
        stdout_of(&output)
    );
    assert!(
        stderr_of(&output).contains("sudo"),
        "the error must tell the operator to use sudo:\n{}",
        stderr_of(&output)
    );
}

// ── the release ─────────────────────────────────────────────────────────────

/// The release has to publish the installer and the two units — the
/// one-liner resolves them through `releases/latest/download`.
#[test]
fn release_publishes_the_installer_and_the_units() {
    let workflow = read_repo_file(".github/workflows/release.yml");

    // The publishing job needs the repository's files to copy from.
    let release_job = workflow
        .split("  release:")
        .nth(1)
        .expect("the release job of the workflow");
    assert!(
        release_job.contains("actions/checkout"),
        "the publishing job must check the repository out — the installer and the units are \
         repository files"
    );

    for asset in [
        "dist/install.sh",
        "dist/pangolin-ngx.service",
        "dist/pangolin-tun.service",
    ] {
        assert!(
            release_job.contains(asset),
            "the release must publish {asset}"
        );
    }

    // The checksum file has to cover the three new assets as well —
    // the installer verifies them against it, so a checksum file
    // without them breaks every installation.
    //
    // The assertion is on the `sha256sum` invocation itself, not on
    // the step as a whole: the assets appear in the staging step and
    // in the publish list as well, so a substring check over the whole
    // job would stay green when the checksum command loses them.
    let checksum_step = step_script(&workflow, "Generate checksums");
    let hashed = checksum_step
        .split_once("sha256sum")
        .unwrap_or_else(|| panic!("the checksum step must run sha256sum:\n{checksum_step}"))
        .1
        .split('>')
        .next()
        .unwrap_or_default();
    assert!(
        hashed.contains("pangolin_*.tar.gz") && hashed.contains("pangolin_*.zip"),
        "the checksum step must keep hashing the archives:\n{checksum_step}"
    );
    for asset in ["install.sh", "pangolin-ngx.service", "pangolin-tun.service"] {
        assert!(
            hashed.contains(asset),
            "the checksum file must cover {asset}:\n{checksum_step}"
        );
    }

    // And the units are copied from the repository, not written twice.
    for (source, destination) in [
        (
            "deploy/playbooks/roles/ngx/files/ngx.service",
            "dist/pangolin-ngx.service",
        ),
        (
            "deploy/playbooks/roles/tun/files/tun.service",
            "dist/pangolin-tun.service",
        ),
        ("install.sh", "dist/install.sh"),
    ] {
        assert!(
            release_job.contains(&format!("{source} {destination}")),
            "the release must copy {source} to {destination} — the repository file is the \
             single source"
        );
    }
}

/// The units the release publishes are the repository's own files, and
/// they point at the install directory the installer uses.
#[test]
fn the_release_units_are_the_repository_files() {
    if !prerequisites_available() {
        eprintln!("sh or curl is missing — skipping");
        return;
    }

    let ngx_unit = read_repo_file("deploy/playbooks/roles/ngx/files/ngx.service");
    let tun_unit = read_repo_file("deploy/playbooks/roles/tun/files/tun.service");

    assert!(
        ngx_unit.contains(&format!(
            "ExecStart={DEFAULT_INSTALL_DIR}/pangolin-ngx --config {DEFAULT_INSTALL_DIR}/ngx.yml"
        )),
        "the gateway unit must start the gateway of the default install directory:\n{ngx_unit}"
    );
    assert!(
        tun_unit.contains(&format!(
            "ExecStart={DEFAULT_INSTALL_DIR}/pangolin-tun --config {DEFAULT_INSTALL_DIR}/tun.yml"
        )),
        "the tunnel unit must start the tunnel node of the default install directory:\n{tun_unit}"
    );

    // The unit the installer writes is the repository's file, with the
    // install directory folded in — nothing else is changed, so the two
    // cannot drift apart.
    let release = FakeRelease::new();
    let sandbox = Sandbox::new();
    let output = sandbox.run(&release.base_url, &[("PANGOLIN_VERSION", "v1.2.3")]);
    assert!(
        output.status.success(),
        "the install failed:\n{}{}",
        stdout_of(&output),
        stderr_of(&output)
    );

    for (unit, original) in [
        ("pangolin-ngx.service", &ngx_unit),
        ("pangolin-tun.service", &tun_unit),
    ] {
        let installed =
            fs::read_to_string(sandbox.systemd_dir.join(unit)).expect("read the installed unit");
        let restored = installed.replace(
            &sandbox.install_dir.to_string_lossy().into_owned(),
            DEFAULT_INSTALL_DIR,
        );
        assert_eq!(
            restored, *original,
            "the installed {unit} must be the repository's file, with only the install \
             directory folded in"
        );
    }
}
