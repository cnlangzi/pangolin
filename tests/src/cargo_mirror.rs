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
/// The trailing-newline variant matters: the managed block is written
/// at the top of the file precisely so that removing it restores the
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
/// so it must contain nothing but `[source.*]` tables — anything else
/// could collide with a table the host already defines and make cargo
/// fail to parse the whole file.
#[test]
fn mirror_config_only_defines_source_tables() {
    let cfg = parse_toml("build/cargo-mirror.toml");
    let source = cfg.get("source").expect("[source.*] tables");
    let table = source.as_table().expect("[source.*] is a table");

    for (name, value) in table {
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
                "[source.{name}] has an unexpected key `{key}` — the block is injected into the \
                 host cargo config, where an unknown key could collide with the host's own tables"
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

/// A host config that already defines a `[source.*]` table must be
/// refused, with the offending table named — and nothing may be
/// written.  A second `[source.crates-io]` would make cargo fail to
/// parse the whole file, breaking every cargo command on the machine.
#[test]
fn installer_refuses_a_host_config_that_already_defines_a_source_table() {
    let host = Host::new();
    let original = format!(
        "{}\n[source.other]\nregistry = \"sparse+https://example.com/index/\"\n",
        host_config(true)
    );
    host.write_config(&original);

    let out = host.run("install");
    assert!(
        !out.status.success(),
        "the installer must refuse a host config that already defines a [source.*] table"
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("[source.other]"),
        "the message must name the offending table:\n{stderr}"
    );
    assert_eq!(
        host.read_config(),
        original,
        "nothing may be written when the installer refuses"
    );
}

/// Installing and then removing the block must give back the host
/// config byte-for-byte — including a file that did not end with a
/// newline, which is the case the block's top-of-file placement
/// exists for.
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

/// Switching mirrors leaves the previous source's registry cache
/// directories behind.  The installer reports them and deletes
/// nothing — removing hundreds of megabytes of the user's cache
/// unasked would be destructive.
#[test]
fn installer_reports_the_stale_registry_cache_and_deletes_nothing() {
    let host = Host::new();
    let cache = host.dir.path().join("registry/cache");
    let stale = [
        "index.crates.io-1949cf8c6b5b557f",
        "mirrors.sjtug.sjtu.edu.cn-a0dcbb55c5c70240",
    ];
    for name in stale {
        fs::create_dir_all(cache.join(name)).expect("create cache dir");
    }
    // The active mirror's own cache directory is not stale.
    let active = "rsproxy.cn-e3de039b2554c837";
    fs::create_dir_all(cache.join(active)).expect("create cache dir");

    let stdout = host.run_ok("install");

    for name in stale {
        assert!(
            stdout.contains(name),
            "the installer must report the stale cache directory `{name}`:\n{stdout}"
        );
        assert!(
            cache.join(name).is_dir(),
            "the installer must not delete `{name}`"
        );
    }
    assert!(
        !stdout.contains(active),
        "the active mirror's own cache directory is not stale:\n{stdout}"
    );
}

/// `status` reports the block and the active mirror without touching
/// anything.
#[test]
fn installer_status_reports_the_active_mirror() {
    let host = Host::new();
    let original = host_config(true);
    host.write_config(&original);

    let before = host.run_ok("status");
    assert!(
        before.contains("rsproxy"),
        "status must name the active mirror:\n{before}"
    );
    assert_eq!(
        host.read_config(),
        original,
        "status must not write anything"
    );

    host.run_ok("install");
    let after = host.run_ok("status");
    assert!(
        after.contains("rsproxy"),
        "status must name the active mirror:\n{after}"
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
