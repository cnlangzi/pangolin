//! Regression tests for the `pangolin-tun` dependency swap (issue #159).
//!
//! The tunnel's wire protocol and its client used to live in this
//! repository — the protocol at `crates/pangolin-core/src/tunnel.rs`
//! (plus `crates/pangolin-core/src/tunnel/`) and the client at
//! `crates/tun/src/client.rs`.  They are now the published crate
//! `pangolin-tun` on crates.io, consumed from the registry, and the
//! in-tree copies are gone.
//!
//! Two copies of one contract drift, and nothing in the compiler
//! notices the drift: the wire format still compiles, the tests still
//! pass, and a gateway and a tun that were not upgraded together
//! fail at runtime instead of at build time.  These tests are the
//! cheap, offline guard against that — they parse the manifests and
//! the lockfile and assert that *this* repository keeps no second
//! copy of the protocol.
//!
//! They deliberately duplicate none of the crate's own tests: the
//! crate's suite asserts the behaviour of the code, these assert the
//! shape of the repository that consumes it.
//!
//! Like the `knownbots` dependency tests (issue #146), they need
//! neither the `integration` feature nor a real binary, so they run
//! in the day-to-day `cargo test --workspace` / `make lint` cycle.

use std::path::{Path, PathBuf};

use crate::common::{parse_toml, repo_root};

/// The dependency the gateway and the tun consume.
const CRATE: &str = "pangolin-tun";

/// The published artefact's checksum — the `sha256` of the `.crate`
/// file on crates.io.
///
/// A re-publish of 0.1.0 under different content would not resolve to
/// the same checksum, and cargo would refuse the lockfile.
const CHECKSUM: &str = "becd40f4e2de655c0ddc6bbe538bae8c49b5ecea9efc7c16b0847ef8d4118082";

/// The version the lockfile must resolve.
const VERSION: &str = "0.1.0";

/// The `[[package]]` entry of the lockfile for `name`.
fn lock_entry<'a>(packages: &'a [toml::Value], name: &str) -> &'a toml::Value {
    packages
        .iter()
        .find(|p| p.get("name").and_then(|n| n.as_str()) == Some(name))
        .unwrap_or_else(|| panic!("Cargo.lock has no entry for {name}"))
}

/// The `[[package]]` array of the lockfile.
fn lock_packages() -> Vec<toml::Value> {
    parse_toml("Cargo.lock")
        .get("package")
        .and_then(|p| p.as_array())
        .expect("Cargo.lock has a [[package]] array")
        .clone()
}

/// Every Rust file under `dir`, recursively.
fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(rust_files(&path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    out
}

/// The in-tree copy is gone: the protocol module, the client, the
/// directory that would hold a vendored crate and the workspace
/// member that pointed at it.
///
/// This is the failure the whole change is about — a second copy of
/// the protocol creeping back into this repository, where it would
/// drift away from the published crate.
#[test]
fn the_in_tree_tunnel_protocol_is_gone() {
    let root = repo_root();

    assert!(
        !root.join("crates/pangolin-core/src/tunnel.rs").exists(),
        "crates/pangolin-core/src/tunnel.rs must be gone — the protocol is published"
    );
    assert!(
        !root.join("crates/pangolin-core/src/tunnel").exists(),
        "crates/pangolin-core/src/tunnel/ must be gone — the protocol is published"
    );
    assert!(
        !root.join("crates/tun/src/client.rs").exists(),
        "crates/tun/src/client.rs must be gone — the client is published"
    );
    assert!(
        !root.join("crates/pangolin-tun").exists(),
        "crates/pangolin-tun/ must be gone — the crate is published"
    );

    let manifest = parse_toml("Cargo.toml");
    let members = manifest
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
        .expect("Cargo.toml has a [workspace].members array");
    assert!(
        !members
            .iter()
            .filter_map(|m| m.as_str())
            .any(|m| { m.contains("pangolin-tun") || m.contains("pangolin_tun") }),
        "the workspace must not have a pangolin-tun member — the crate is consumed from the registry"
    );
}

/// No file under `crates/` may still reach for the in-tree protocol.
///
/// The module is deleted, so the import would not compile — but the
/// point of this assertion is the *message*: it names the mistake
/// (a leftover import of the deleted module) instead of leaving the
/// reader to work it out from a compiler error somewhere else.
///
/// The match is on the **import form**, not on the bare path: this
/// repository's convention is to name retired paths in prose (a
/// comment explaining what the tunnel's client used to do), and a
/// guard that fired on any occurrence would turn such a sentence into
/// a red build.
#[test]
fn no_crate_imports_the_in_tree_tunnel_module() {
    let root = repo_root();
    let crates_dir = root.join("crates");

    for file in rust_files(&crates_dir) {
        let source = std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        for (number, line) in source.lines().enumerate() {
            let trimmed = line.trim_start();
            let import = trimmed
                .strip_prefix("pub use ")
                .or_else(|| trimmed.strip_prefix("use "));
            let names_the_module = import.is_some_and(|rest| {
                rest.contains("pangolin_core::tunnel") || rest.contains("pangolin_core::{tunnel")
            });
            assert!(
                !names_the_module,
                "{}:{} still imports pangolin_core::tunnel — the protocol is published as pangolin-tun",
                file.strip_prefix(&root).unwrap_or(&file).display(),
                number + 1
            );
        }
    }
}

/// The workspace depends on the **published** crate: a version
/// requirement, not a path.
///
/// The requirement is a caret on the 0.1 line — a future 0.2
/// release would carry a protocol change, so it must not be picked
/// up silently.  A `path` key would mean the in-tree copy crept
/// back.
#[test]
fn the_workspace_depends_on_the_published_crate() {
    let manifest = parse_toml("Cargo.toml");
    let dependency = manifest
        .get("workspace")
        .and_then(|w| w.get("dependencies"))
        .and_then(|d| d.get(CRATE))
        .and_then(|d| d.as_table())
        .unwrap_or_else(|| panic!("the workspace must manage the {CRATE} dependency"));

    assert!(
        !dependency.contains_key("path"),
        "{CRATE} must not be a path dependency — it is published"
    );
    let version = dependency
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("the {CRATE} dependency must carry a version requirement"));
    assert!(
        version.starts_with("0.1"),
        "the {CRATE} dependency must ask for the 0.1 line, got {version:?}"
    );
}

/// Every consumer inherits the dependency — none of them declares a
/// version of its own.
///
/// One place names the version, so a second consumer cannot pick a
/// different one and silently fork the protocol.
#[test]
fn the_consumers_inherit_the_dependency() {
    for consumer in [
        "crates/pangolin-core/Cargo.toml",
        "crates/ngx/Cargo.toml",
        "crates/tun/Cargo.toml",
        "tests/Cargo.toml",
    ] {
        let manifest = parse_toml(consumer);
        let declared = manifest
            .get("dependencies")
            .and_then(|d| d.get(CRATE))
            .and_then(|d| d.as_table())
            .unwrap_or_else(|| panic!("{consumer} must depend on {CRATE}"));

        assert_eq!(
            declared.get("workspace").and_then(|w| w.as_bool()),
            Some(true),
            "{consumer} must inherit the workspace dependency: {declared:?}"
        );
        assert!(
            !declared.contains_key("path"),
            "{consumer} must not point at a path — the crate is published"
        );
        assert!(
            !declared.contains_key("version"),
            "{consumer} must not declare a version of its own — the workspace names it"
        );
    }
}

/// The lockfile resolves the crate from the registry, at the
/// published version.
#[test]
fn the_lockfile_resolves_pangolin_tun_from_the_registry() {
    let packages = lock_packages();
    let entry = lock_entry(&packages, CRATE);

    let source = entry
        .get("source")
        .and_then(|s| s.as_str())
        .unwrap_or_else(|| panic!("{CRATE} must come from a registry, not from a path"));
    assert!(
        source.contains("crates.io-index"),
        "{CRATE} must resolve from crates.io, got {source:?}"
    );
    assert_eq!(
        entry.get("version").and_then(|v| v.as_str()),
        Some(VERSION),
        "the locked {CRATE} must be the published {VERSION}"
    );
}

/// The locked checksum is the published artefact's checksum.
#[test]
fn the_locked_checksum_is_the_published_artefact() {
    let packages = lock_packages();
    let entry = lock_entry(&packages, CRATE);
    let checksum = entry
        .get("checksum")
        .and_then(|c| c.as_str())
        .unwrap_or_else(|| panic!("the registry entry of {CRATE} must carry a checksum"));
    assert_eq!(
        checksum, CHECKSUM,
        "the locked {CRATE} must be the published {VERSION} artefact"
    );
}

/// The gateway and the tun both speak the **same** protocol version.
///
/// The wire format did not change when the protocol moved to the
/// published crate, so a tun and a gateway that were not upgraded
/// together still interoperate.  The constant now lives in the crate;
/// asserting it here pins the value the two ends agree on.
#[test]
fn the_protocol_version_is_still_two() {
    assert_eq!(
        pangolin_tun::TUNNEL_PROTOCOL_VERSION,
        2,
        "the tunnel's protocol version must stay 2 — the wire format did not change"
    );
}
