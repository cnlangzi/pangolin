//! Regression tests for the `knownbots` dependency swap (issue #146).
//!
//! The bot-verification layer used to live in this repository, at
//! `crates/knownbots/`.  It is now the published crate
//! `knownbots` on crates.io, consumed from the registry, and the
//! in-tree copy is gone.
//!
//! The locked version is 0.4.0 (issue #230): the release that owns the
//! category vocabulary — the coarse buckets and their wire labels — so
//! the host consumes the crate's `BotCategory` instead of keeping a
//! second copy of the enum that could drift away from the labels the
//! JSONL rows carry.  0.3.0 was the previous floor (issue #157): the
//! first release whose transport repeats a refused download over the
//! other address family and remembers the family that delivered the
//! list.  0.2.0 leaves the address order to the system resolver, which
//! hands back the AAAA record first — a tampered IPv6 path to a
//! publisher then starves the crawler of its address list, and the
//! crawler is throttled like an ordinary client.
//!
//! Three failure modes are worth a test, and none of them shows up
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
//! 3. **The host grows a second category vocabulary.**  A local
//!    `enum BotCategory` beside the crate's is two sources of truth
//!    for the wire labels the JSONL rows carry, and the two drift.
//!
//! The tests are deliberately cheap and hermetic — they parse the
//! manifests and the lockfile.  No network, no binaries — so they run
//! in the day-to-day `cargo test --workspace` / `make lint` cycle.

use std::path::Path;

use crate::common::{lock_entry, lock_packages, parse_toml, repo_root};

/// The dependency the adapter consumes.
const CRATE: &str = "knownbots";

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
        version.starts_with("0.4"),
        "the {CRATE} dependency must ask for the 0.4 line, got {version:?}"
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
        Some("0.4.0"),
        "the locked {CRATE} must be the published 0.4.0"
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
    // The checksum pins the exact artefact: a re-publish of 0.4.0
    // under a different content would not resolve to the same
    // checksum, and cargo would refuse the lockfile.
    let packages = lock_packages();
    let entry = lock_entry(&packages, CRATE);
    let checksum = entry
        .get("checksum")
        .and_then(|c| c.as_str())
        .unwrap_or_else(|| panic!("the registry entry of {CRATE} must carry a checksum"));
    assert_eq!(
        checksum, "8116fcb36a8a50d9cf04beabe9b809bccdfc736b7c33cfa5cd3c85e56c8d9072",
        "the locked {CRATE} must be the published 0.4.0 artefact"
    );
}

/// The category vocabulary belongs to the crate: the adapter consumes
/// the crate's `BotCategory` and keeps no second copy of the enum, of
/// its `Display`, or of the mapping into the buckets.
///
/// A local copy is the failure this guards: it is a second source of
/// truth for the wire labels the JSONL rows carry, and the two drift —
/// a drifted label makes the downstream queries filter on a value that
/// is no longer written.
#[test]
fn the_adapter_consumes_the_crate_category_vocabulary() {
    let adapter = repo_root().join("crates/pangolin-core/src/bot.rs");
    let source = std::fs::read_to_string(&adapter)
        .unwrap_or_else(|e| panic!("read {}: {e}", adapter.display()));

    assert!(
        source.contains("pub use knownbots::BotCategory;"),
        "the adapter must re-export the crate's category type"
    );
    assert!(
        !source.contains("enum BotCategory"),
        "the adapter must not define a second category vocabulary"
    );
    assert!(
        !source.contains("fn category_for("),
        "the mapping into the buckets belongs to the crate"
    );
    assert!(
        !source.contains("impl std::fmt::Display for BotCategory"),
        "the wire label is the crate's — the adapter must not print its own"
    );
    assert!(
        !Path::new(&repo_root().join("crates/knownbots")).exists(),
        "there is no in-tree copy left to carry the vocabulary"
    );
}
