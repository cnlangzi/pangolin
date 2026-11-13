//! Regression tests for the `knownbots` dependency swap (issue #146).
//!
//! The bot-verification layer used to live in this repository, at
//! `crates/knownbots/`.  It is now the published crate
//! `knownbots` on crates.io, consumed from the registry, and the
//! in-tree copy is gone.
//!
//! Two failure modes are worth a test, and neither of them shows up
//! as a compile error:
//!
//! 1. **The `full` feature is dropped.**  The crate's default feature
//!    set is empty; without `full` the IP-list download and the RDNS
//!    workers are not compiled in, and every URL-backed bot stays
//!    `Cold` — the gateway keeps serving, it just stops recognising
//!    the crawlers that publish an address list.
//! 2. **The in-tree copy comes back** (a path dependency, a workspace
//!    member, a stray directory).  Two copies of the same registry
//!    drift, and the whole point of publishing the crate is that there
//!    is one source of truth.
//!
//! The tests are deliberately cheap and hermetic — they parse the
//! manifests and the lockfile.  No network, no binaries — so they run
//! in the day-to-day `cargo test --workspace` / `make lint` cycle.

use std::path::Path;

use crate::common::{parse_toml, repo_root};

/// The dependency the adapter consumes.
const CRATE: &str = "knownbots";

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

/// The in-tree copy is gone: the directory, and the workspace member
/// that pointed at it.
#[test]
fn the_in_tree_knownbots_crate_is_gone() {
    let root = repo_root();
    assert!(
        !root.join("crates/knownbots").exists(),
        "crates/knownbots/ must be gone — the crate is published"
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
            .any(|m| m.contains("knownbots")),
        "the workspace must not have a knownbots member — the crate is consumed from the registry"
    );
}

/// The adapter depends on the **published** crate: a version
/// requirement, not a path.
///
/// The version and the feature set are workspace-managed — one place
/// names them, so a second consumer cannot pick a different pair.
/// `pangolin-core` therefore inherits them (`knownbots.workspace =
/// true`), and a `path` key would mean the in-tree copy crept back.
#[test]
fn pangolin_core_depends_on_the_published_crate() {
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

    // The consumer inherits it — it does not declare its own.
    let consumer = parse_toml("crates/pangolin-core/Cargo.toml");
    let declared = consumer
        .get("dependencies")
        .and_then(|d| d.get(CRATE))
        .and_then(|d| d.as_table())
        .unwrap_or_else(|| panic!("pangolin-core must depend on {CRATE}"));
    assert_eq!(
        declared.get("workspace").and_then(|w| w.as_bool()),
        Some(true),
        "pangolin-core must inherit the workspace dependency: {declared:?}"
    );
    assert!(
        !declared.contains_key("path"),
        "pangolin-core must not point at a path — the crate is published"
    );
}

/// The `full` feature is enabled.  The crate's default feature set is
/// empty, so without it the IP-list download and the RDNS workers are
/// not compiled in — and every URL-backed bot stays `Cold`.
#[test]
fn the_full_feature_is_enabled() {
    let manifest = parse_toml("Cargo.toml");
    let dependency = manifest
        .get("workspace")
        .and_then(|w| w.get("dependencies"))
        .and_then(|d| d.get(CRATE))
        .and_then(|d| d.as_table())
        .expect("the workspace must manage the knownbots dependency");

    let features = dependency
        .get("features")
        .and_then(|f| f.as_array())
        .unwrap_or_else(|| panic!("the knownbots dependency must enable the `full` feature"));
    assert!(
        features.iter().any(|f| f.as_str() == Some("full")),
        "the knownbots dependency must enable `full`, got {features:?}"
    );
}

/// The lockfile resolves the crate from the registry, and the
/// optional dependencies of `full` are part of the resolved graph —
/// the cheap, offline proof that the feature is on.
#[test]
fn the_lockfile_resolves_knownbots_from_the_registry() {
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
        Some("0.1.0"),
        "the locked {CRATE} must be the published 0.1.0"
    );

    let dependencies = entry
        .get("dependencies")
        .and_then(|d| d.as_array())
        .expect("the lockfile lists the dependencies of the package");
    for optional in ["dns-lookup", "reqwest", "tokio"] {
        // Cargo writes `name` alone while one version of the crate is
        // in the graph, and `name version` as soon as a second one
        // appears. Both forms mean the same dependency.
        assert!(
            dependencies.iter().any(|d| {
                d.as_str()
                    .is_some_and(|d| d == optional || d.starts_with(&format!("{optional} ")))
            }),
            "the `full` feature pulls in {optional} — it is missing from the lockfile"
        );
    }
}

/// The crate the adapter talks to is the published one, under the
/// name cargo resolves it to.
#[test]
fn the_published_crate_is_the_one_on_the_registry() {
    // The checksum pins the exact artefact: a re-publish of 0.1.0
    // under a different content would not resolve to the same
    // checksum, and cargo would refuse the lockfile.
    let packages = lock_packages();
    let entry = lock_entry(&packages, CRATE);
    let checksum = entry
        .get("checksum")
        .and_then(|c| c.as_str())
        .unwrap_or_else(|| panic!("the registry entry of {CRATE} must carry a checksum"));
    assert_eq!(
        checksum, "fe31123efc4b7d056b37345623363f24405ab6aa6ca827facdec9e9007f0400d",
        "the locked {CRATE} must be the published 0.1.0 artefact"
    );
}

/// The library must stay free of pangolin's admin vocabulary: the
/// category mapping belongs to the adapter, not to the crate.
#[test]
fn the_adapter_owns_the_category_mapping() {
    let adapter = repo_root().join("crates/pangolin-core/src/bot.rs");
    let source = std::fs::read_to_string(&adapter)
        .unwrap_or_else(|e| panic!("read {}: {e}", adapter.display()));

    assert!(
        source.contains("fn category_for("),
        "the category mapping must live in the adapter"
    );
    assert!(
        !source.contains("from_label"),
        "the wire-label round-trip is gone — the mapping is derived from the BotKind"
    );
    assert!(
        !Path::new(&repo_root().join("crates/knownbots")).exists(),
        "there is no in-tree copy left to carry the admin vocabulary"
    );
}
