//! Regression tests for the crates.io mirror configuration (issue #126).
//!
//! The bug these pin: the mirror was configured through cargo's
//! `[registries]` tables (`[registries.crates-io] replace-with`).
//! Cargo's resolver **ignores** `replace-with` there — silently, with
//! no warning — so every build resolved against the real crates.io
//! instead of the configured mirror.  Source replacement only works
//! through the `[source]` tables, which is what
//! `build/cargo-mirror.toml` now uses.
//!
//! These tests are deliberately cheap and hermetic: they parse the
//! two config files, check the Dockerfile's step order, and drive
//! `build/cargo-mirror.sh` against a throwaway `CARGO_HOME`.  No
//! network, no Docker, no pangolin binaries — so they run in the
//! day-to-day `cargo test --workspace` / `make lint` cycle.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

/// The marker lines the installer uses to delimit its managed block.
const MARK_START: &str = "# >>> pangolin cargo mirror >>>";
const MARK_END: &str = "# <<< pangolin cargo mirror <<<";

/// A host cargo config shaped like the ones developers actually have:
/// a `[net]` table and two `[url.*] insteadOf` rules.
///
/// The trailing-newline variant matters: the managed block is appended
/// at the end of the file precisely so that removing it restores the
/// original bytes — including the absence of a final newline, which
/// is how the file on this very machine looks.
const HOST_CONFIG_BODY: &str = concat!(
    "[net]\n",
    "git-fetch-with-cli = true\n",
    "\n",
    "[url.\"https://github.com/\"]\n",
    "insteadOf = \"git@github.com:\"\n",
    "\n",
    "[url.\"ssh://git@github.com/\"]\n",
    "insteadOf = \"ssh://git@github.com/\"",
);

fn host_config(trailing_newline: bool) -> String {
    let mut out = String::from(HOST_CONFIG_BODY);
    if trailing_newline {
        out.push('\n');
    }
    out
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tests/ lives directly under the repo root")
        .to_path_buf()
}

fn read_repo_file(relative: &str) -> String {
    let path = repo_root().join(relative);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn parse_toml(relative: &str) -> toml::Value {
    let path = repo_root().join(relative);
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    toml::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

fn str_field<'a>(value: &'a toml::Value, key: &str) -> &'a str {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("expected string field `{key}` in {value:?}"))
}

/// The mirror `replace-with` selects, as `<name>` and the registry
/// host cargo keys its cache directories by — e.g. `("rsproxy",
/// "rsproxy.cn")`.  Read from the canonical file so the tests follow
/// the mirror when it is switched.
fn active_mirror() -> (String, String) {
    let cfg = parse_toml("build/cargo-mirror.toml");
    let source = cfg.get("source").expect("[source.*] tables");
    let name = source
        .get("crates-io")
        .and_then(|c| c.get("replace-with"))
        .and_then(|v| v.as_str())
        .expect("[source.crates-io].replace-with")
        .to_string();
    let registry = str_field(
        source
            .get(&name)
            .unwrap_or_else(|| panic!("[source.{name}] must be defined")),
        "registry",
    );
    let host = registry
        .trim_start_matches("sparse+https://")
        .split('/')
        .next()
        .expect("registry host")
        .to_string();
    (name, host)
}

// ── the canonical mirror block ─────────────────────────────────────────────

/// The mirror must be configured through `[source]` tables.  The
/// `[registries]` form looks right and is silently ignored, which is
/// the bug this whole change is about.
#[test]
fn mirror_config_uses_source_replacement_not_registries() {
    let cfg = parse_toml("build/cargo-mirror.toml");

    assert!(
        cfg.get("registries").is_none(),
        "build/cargo-mirror.toml must not use the [registries] tables: cargo's resolver \
         ignores `replace-with` there, so the mirror would silently never apply"
    );

    let source = cfg
        .get("source")
        .expect("build/cargo-mirror.toml must define [source.*] tables");

    assert_eq!(
        source
            .get("crates-io")
            .and_then(|c| c.get("replace-with"))
            .and_then(|v| v.as_str()),
        Some("rsproxy"),
        "rsproxy is the default mirror; switching it is a one-line edit of replace-with"
    );

    for name in ["rsproxy", "sjtu", "ustc", "tuna"] {
        let mirror = source
            .get(name)
            .unwrap_or_else(|| panic!("build/cargo-mirror.toml must define [source.{name}]"));
        let registry = str_field(mirror, "registry");
        assert!(
            registry.starts_with("sparse+https://"),
            "[source.{name}].registry must be a sparse registry URL, got {registry}"
        );
    }
}

/// The managed block is injected as-is into the host's cargo config,
/// so the file must contain *nothing* but `[source.*]` tables.  A
/// stray `[net]` would be written into the host config, where it
/// collides with the host's own `[net]` — TOML forbids defining the
/// same table twice, and cargo then fails to parse the whole file.
#[test]
fn mirror_config_only_defines_source_tables() {
    let cfg = parse_toml("build/cargo-mirror.toml");

    let top_level: Vec<&String> = cfg.as_table().expect("a table").keys().collect();
    assert_eq!(
        top_level,
        vec!["source"],
        "build/cargo-mirror.toml must define exactly one top-level table, [source] — \
         anything else is injected into the host's cargo config and can collide with \
         a table the host already defines"
    );

    let source = cfg.get("source").expect("[source.*] tables");
    for (name, value) in source.as_table().expect("[source.*] is a table") {
        assert!(
            value.is_table(),
            "[source.{name}] must be a table, got {value:?}"
        );
        for key in value.as_table().expect("table").keys() {
            assert!(
                matches!(
                    key.as_str(),
                    "registry" | "replace-with" | "branch" | "tag" | "rev"
                ),
                "[source.{name}] has an unexpected key `{key}` — the block is injected into \
                 the host cargo config, where an unknown key could collide with the \
                 host's own tables"
            );
        }
    }
}

// ── the Docker-only cargo settings ─────────────────────────────────────────

/// The Docker-only file keeps the linker and network settings and no
/// longer carries a mirror block.  A `[registries.*]` table here would
/// be the old broken form all over again.
#[test]
fn docker_cargo_config_keeps_only_docker_specific_settings() {
    let cfg = parse_toml("build/docker/cargo-config.toml");

    assert!(
        cfg.get("registries").is_none(),
        "build/docker/cargo-config.toml must not carry a [registries.*] table — the mirror \
         lives in build/cargo-mirror.toml"
    );

    let target = cfg
        .get("target")
        .and_then(|t| t.get("x86_64-unknown-linux-gnu"))
        .expect("[target.x86_64-unknown-linux-gnu]");
    assert_eq!(str_field(target, "linker"), "clang");
    let rustflags = target
        .get("rustflags")
        .and_then(|v| v.as_array())
        .expect("rustflags array");
    assert_eq!(
        rustflags
            .iter()
            .map(|v| v.as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        vec!["-C", "link-arg=-fuse-ld=mold"]
    );

    let net = cfg.get("net").expect("[net]");
    assert_eq!(
        net.get("git-fetch-with-cli").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(net.get("retry").and_then(|v| v.as_integer()), Some(10));

    let http = cfg.get("http").expect("[http]");
    assert_eq!(
        http.get("multiplexing").and_then(|v| v.as_bool()),
        Some(false)
    );
    // 600 s × 10 retries meant one stalled transfer burned 648 s and a
    // build sat there for hours.
    assert_eq!(http.get("timeout").and_then(|v| v.as_integer()), Some(120));
    assert_eq!(
        http.get("low-speed-limit").and_then(|v| v.as_integer()),
        Some(1)
    );
}

// ── the Dockerfile ─────────────────────────────────────────────────────────

/// The image's cargo config must be assembled from BOTH files, and it
/// must be in place before the two `cargo install` steps — otherwise
/// those installs resolve against crates.io instead of the mirror.
#[test]
fn dockerfile_assembles_the_cargo_config_before_the_cargo_install_steps() {
    let text = read_repo_file("build/docker/dist.dockerfile");
    // Only real instructions count — the comments around them name the
    // same steps, and matching those would test the prose, not the
    // build.
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect();

    let find = |needle: &str| {
        lines
            .iter()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("dist.dockerfile has no instruction containing `{needle}`"))
    };

    let copy = find("COPY build/cargo-mirror.toml build/docker/cargo-config.toml");
    let write = find("/usr/local/cargo/config.toml");
    let chef = find("cargo install cargo-chef");
    let sccache = find("cargo install sccache");

    assert!(
        copy < write,
        "both config files are copied before the config is written"
    );
    assert!(
        write < chef && write < sccache,
        "the cargo config must be written before the cargo install steps, so those resolve \
         through the mirror"
    );

    // The write must actually use BOTH files.  Ordering alone would
    // happily pass a `cat` that drops the mirror — which is the #126
    // bug itself.
    let write_line = lines[write];
    assert!(
        write_line.contains("cargo-mirror.toml"),
        "the cargo config must be assembled from build/cargo-mirror.toml: {write_line}"
    );
    assert!(
        write_line.contains("cargo-config.toml"),
        "the cargo config must be assembled from build/docker/cargo-config.toml: {write_line}"
    );
}

// ── the host installer ─────────────────────────────────────────────────────

/// A throwaway `CARGO_HOME` plus a helper to run the installer
/// against it.
struct Host {
    dir: TempDir,
}

impl Host {
    fn new() -> Self {
        Self {
            dir: TempDir::new().expect("temp dir"),
        }
    }

    fn config_path(&self) -> PathBuf {
        self.dir.path().join("config.toml")
    }

    fn write_config(&self, contents: &str) {
        fs::write(self.config_path(), contents).expect("write host config");
    }

    fn read_config(&self) -> String {
        fs::read_to_string(self.config_path()).expect("read host config")
    }

    fn run(&self, mode: &str) -> Output {
        Command::new("bash")
            .arg(repo_root().join("build/cargo-mirror.sh"))
            .arg(mode)
            .env("CARGO_HOME", self.dir.path())
            // The script falls back to $HOME/.cargo when CARGO_HOME is
            // unset; keep the fallback inside the sandbox too.
            .env("HOME", self.dir.path())
            .output()
            .expect("run build/cargo-mirror.sh")
    }

    fn run_ok(&self, mode: &str) -> String {
        let out = self.run(mode);
        assert!(
            out.status.success(),
            "`cargo-mirror.sh {mode}` failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

/// Installing twice must converge on exactly one managed block, and the
/// host's own tables must survive untouched.
#[test]
fn installer_install_is_idempotent_and_preserves_the_host_config() {
    let host = Host::new();
    host.write_config(&host_config(true));

    host.run_ok("install");
    let after_first = host.read_config();

    host.run_ok("install");
    let after_second = host.read_config();

    assert_eq!(
        after_first, after_second,
        "a second install must not change the file"
    );
    assert_eq!(
        count_occurrences(&after_second, MARK_START),
        1,
        "exactly one managed block must be present:\n{after_second}"
    );
    assert_eq!(count_occurrences(&after_second, MARK_END), 1);

    for expected in [
        "[net]",
        "git-fetch-with-cli = true",
        "[url.\"https://github.com/\"]",
        "[url.\"ssh://git@github.com/\"]",
        "insteadOf = \"git@github.com:\"",
        "insteadOf = \"ssh://git@github.com/\"",
    ] {
        assert!(
            after_second.contains(expected),
            "the host config must keep `{expected}`:\n{after_second}"
        );
    }
}

/// The managed block goes at the END of the host config.  TOML binds a
/// table-less key to the most recent table header, so a block in front
/// of the host's own tables would swallow the host's root-level keys
/// (`credential-provider` and friends) and silently drop them.
#[test]
fn installer_appends_the_block_so_root_level_keys_survive() {
    let host = Host::new();
    let original = "credential-provider = \"cargo:token\"\n\n[net]\ngit-fetch-with-cli = true\n";
    host.write_config(original);

    host.run_ok("install");
    let after = host.read_config();

    assert!(
        after.starts_with("credential-provider = \"cargo:token\""),
        "the host's table-less keys must stay at the root of the file, ahead of any table \
         header:\n{after}"
    );
    let block_at = after.find(MARK_START).expect("managed block");
    let net_at = after.find("[net]").expect("[net]");
    assert!(
        net_at < block_at,
        "the managed block must be appended after the host's own tables:\n{after}"
    );

    // …and the root-level key must still parse as a root-level key,
    // not as a member of the block's last table.
    let parsed: toml::Value =
        toml::from_str(&after).unwrap_or_else(|e| panic!("the written config must parse: {e}"));
    assert_eq!(
        parsed.get("credential-provider").and_then(|v| v.as_str()),
        Some("cargo:token"),
        "the root-level key must survive the install:\n{after}"
    );
}

/// A host config that already defines a `[source.*]` table must be
/// refused, with the offending table named — and nothing may be
/// written.  A second `[source.crates-io]` would make cargo fail to
/// parse the whole file, breaking every cargo command on the machine.
///
/// TOML allows whitespace inside the brackets, so `[ source.x ]` is
/// the same table as `[source.x]`.
#[test]
fn installer_refuses_a_host_config_that_already_defines_a_source_table() {
    for table in ["[source.other]", "[ source.crates-io ]", "[source]"] {
        let host = Host::new();
        let original = format!(
            "{}\n{table}\nregistry = \"sparse+https://example.com/index/\"\n",
            host_config(true)
        );
        host.write_config(&original);

        let out = host.run("install");
        assert!(
            !out.status.success(),
            "the installer must refuse a host config that already defines {table}"
        );

        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(table),
            "the message must name the offending table {table}:\n{stderr}"
        );
        assert_eq!(
            host.read_config(),
            original,
            "nothing may be written when the installer refuses"
        );
    }
}

/// A config whose markers are malformed must be refused, not
/// half-rewritten.  An end marker without a start marker — or a start
/// marker without an end marker — leaves the real content of the file
/// unknown, and guessing it would delete the user's cargo config.
#[test]
fn installer_refuses_a_config_with_malformed_markers() {
    for broken in [
        // A start marker with no end marker: everything after it would
        // look like part of the block.
        format!(
            "{}\n{MARK_START}\n\n[http]\ntimeout = 30\n",
            host_config(true)
        ),
        // An end marker with no start marker.
        format!("{}\n{MARK_END}\n", host_config(true)),
    ] {
        let host = Host::new();
        host.write_config(&broken);

        for mode in ["install", "off"] {
            let out = host.run(mode);
            assert!(
                !out.status.success(),
                "`{mode}` must refuse a config with malformed markers:\n{broken}"
            );
            assert_eq!(
                host.read_config(),
                broken,
                "`{mode}` must not write anything when it refuses"
            );
        }
    }
}

/// Installing and then removing the block must give back the host
/// config byte-for-byte — including a file that did not end with a
/// newline, which is the case the block's placement exists for.
#[test]
fn installer_off_restores_the_host_config_byte_for_byte() {
    for original in [host_config(true), host_config(false)] {
        let host = Host::new();
        host.write_config(&original);

        host.run_ok("install");
        assert_ne!(
            host.read_config(),
            original,
            "the install must actually change the file"
        );

        host.run_ok("off");
        assert_eq!(
            host.read_config(),
            original,
            "`off` must restore the host config byte-for-byte"
        );
    }
}

/// Switching mirrors leaves the previous source's registry directories
/// behind — under `registry/cache`, `registry/index` and
/// `registry/src`.  The installer reports them and deletes nothing —
/// removing hundreds of megabytes of the user's cache unasked would be
/// destructive.
#[test]
fn installer_reports_the_stale_registry_directories_and_deletes_nothing() {
    let host = Host::new();
    let (_, host_name) = active_mirror();

    let stale = [
        ("cache", "index.crates.io-1949cf8c6b5b557f"),
        ("index", "index.crates.io-6f17d22bba15001f"),
        ("src", "index.crates.io-1949cf8c6b5b557f"),
    ];
    for (kind, name) in stale {
        fs::create_dir_all(host.dir.path().join("registry").join(kind).join(name))
            .expect("create registry dir");
    }
    // The active mirror's own directories are not stale.
    for kind in ["cache", "index", "src"] {
        fs::create_dir_all(
            host.dir
                .path()
                .join("registry")
                .join(kind)
                .join(format!("{host_name}-e3de039b2554c837")),
        )
        .expect("create registry dir");
    }

    let stdout = host.run_ok("install");

    for (kind, name) in stale {
        assert!(
            stdout.contains(name),
            "the installer must report the stale registry/{kind} directory `{name}`:\n{stdout}"
        );
        assert!(
            host.dir
                .path()
                .join("registry")
                .join(kind)
                .join(name)
                .is_dir(),
            "the installer must not delete registry/{kind}/{name}"
        );
    }
    assert!(
        !stdout.contains(&format!("{host_name}-e3de039b2554c837")),
        "the active mirror's own directories are not stale:\n{stdout}"
    );
}

/// A symlinked config must stay a symlink.  The installer writes
/// through a temp file and renames it into place, which would replace
/// the link with a regular file and silently disconnect a
/// dotfiles-managed config.
#[test]
fn installer_follows_a_symlinked_config_instead_of_replacing_it() {
    let host = Host::new();
    let target = host.dir.path().join("dotfiles-cargo.toml");
    fs::write(&target, host_config(true)).expect("write target");
    std::os::unix::fs::symlink(&target, host.config_path()).expect("symlink");

    host.run_ok("install");

    assert!(
        fs::symlink_metadata(host.config_path())
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false),
        "the config must still be a symlink after install"
    );
    assert!(
        fs::read_to_string(&target).unwrap().contains(MARK_START),
        "the block must be written to the symlink's target"
    );
}

/// `status` reports the block and the active mirror without touching
/// anything.
#[test]
fn installer_status_reports_the_active_mirror() {
    let (name, host_name) = active_mirror();
    let host = Host::new();
    let original = host_config(true);
    host.write_config(&original);

    let before = host.run_ok("status");
    assert!(
        before.contains(&format!("active mirror : {name} ({host_name})")),
        "status must name the active mirror:\n{before}"
    );
    assert!(
        before.contains("managed block : no"),
        "status must report that the block is not installed yet:\n{before}"
    );
    assert_eq!(
        host.read_config(),
        original,
        "status must not write anything"
    );

    host.run_ok("install");
    let after = host.run_ok("status");
    assert!(
        after.contains(&format!("active mirror : {name} ({host_name})")),
        "status must name the active mirror:\n{after}"
    );
    assert!(
        after.contains("managed block : yes"),
        "status must report that the block is installed:\n{after}"
    );
}

// ── the Makefile ───────────────────────────────────────────────────────────

#[test]
fn makefile_exposes_the_mirror_targets() {
    let text = read_repo_file("Makefile");
    assert!(
        text.contains("\ncargo-mirror:\n"),
        "the Makefile must expose a `cargo-mirror` target"
    );
    assert!(
        text.contains("\ncargo-mirror-off:\n"),
        "the Makefile must expose a `cargo-mirror-off` target"
    );
}

/// The documentation is part of the deliverable: the shared file, the
/// host target and the cache-mount behaviour are all recorded in
/// AGENTS.md.
#[test]
fn agents_md_documents_the_shared_mirror_configuration() {
    let text = read_repo_file("AGENTS.md");
    assert!(
        text.contains("build/cargo-mirror.toml"),
        "AGENTS.md must document the shared mirror configuration"
    );
    assert!(
        text.contains("cargo-mirror"),
        "AGENTS.md must document the host installer target"
    );
    assert!(
        text.contains("/usr/local/cargo/registry")
            && text.contains("/pangolin/target")
            && text.contains("/root/.cache/sccache"),
        "AGENTS.md must document the BuildKit cache mounts"
    );
    assert!(
        text.contains("docker builder prune"),
        "AGENTS.md must document that `docker builder prune` clears the cache mounts"
    );
}
