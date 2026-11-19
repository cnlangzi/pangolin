//! Regression tests for the pingora dependency swap (issue #189).
//!
//! The gateway used to depend on a **fork** of pingora —
//! `github.com/cnlangzi/pingora`, branch `pangolin` — wired in
//! through a `[patch.crates-io]` table in the root `Cargo.toml`.
//! The fork carried exactly one local patch (`take_stream()` on
//! `HttpSession`), and that patch became dead code with the tunnel
//! splice rewrite (issues #153 / #154): nothing in this repository
//! calls it any more.
//!
//! The switch to the published crates.io release failed **silently**
//! the first time it was tried, and that is what these tests pin.
//! Cargo accepts a `[patch]` entry whose version does not satisfy the
//! dependency requirement, prints a warning, and resolves the
//! dependency from crates.io anyway — so a manifest that still
//! carried the patch table built against crates.io pingora 0.8.x
//! while the fork contributed nothing, and nobody noticed.  The
//! version requirement and the patch table have to be checked
//! together, every time, because neither one alone is enough.
//!
//! The tests are deliberately cheap and hermetic — they parse the
//! manifests and the lockfile.  No network, no binaries — so they run
//! in the day-to-day `cargo test --workspace` / `make lint` cycle,
//! exactly like the `knownbots` (issue #146) and `pangolin-tun`
//! (issue #159) dependency tests.
//!
//! The policy these tests enforce is written down in
//! `docs/design/pingora-dependency.md`.

use std::path::Path;

use crate::common::{parse_toml, repo_root};

/// The dependency the gateway is built on.
const CRATE: &str = "pingora";

/// The version line the manifests require.
const VERSION_LINE: &str = "0.9";

/// The fork this repository used to consume.  Nothing may resolve
/// from it any more.
const FORK: &str = "cnlangzi/pingora";

/// The `[[package]]` array of the lockfile.
fn lock_packages() -> Vec<toml::Value> {
    parse_toml("Cargo.lock")
        .get("package")
        .and_then(|p| p.as_array())
        .expect("Cargo.lock has a [[package]] array")
        .clone()
}

/// Every `pingora*` package in the lockfile.
fn pingora_packages(packages: &[toml::Value]) -> Vec<&toml::Value> {
    packages
        .iter()
        .filter(|p| {
            p.get("name")
                .and_then(|n| n.as_str())
                .is_some_and(|n| n.starts_with(CRATE))
        })
        .collect()
}

/// The root manifest carries **no** `[patch.crates-io]` table.
///
/// A patch table is the only way a git dependency can enter the
/// graph, and its failure mode is silent: cargo ignores a patch
/// whose version does not match the dependency requirement and
/// resolves from crates.io instead, printing only a warning.  The
/// message names the fork so the reader knows which dependency the
/// stale table was about.
#[test]
fn the_root_manifest_has_no_patch_table() {
    let manifest = parse_toml("Cargo.toml");
    assert!(
        manifest
            .get("patch")
            .and_then(|p| p.get("crates-io"))
            .is_none(),
        "the root Cargo.toml must not carry a [patch.crates-io] table — \
         pingora comes from crates.io, not from the {FORK} fork"
    );
    // The table can also be spelled as a dotted key.
    assert!(
        !manifest
            .as_table()
            .is_some_and(|t| t.contains_key("patch.crates-io")),
        "the root Cargo.toml must not carry a [patch.crates-io] table — \
         pingora comes from crates.io, not from the {FORK} fork"
    );
}

/// The workspace depends on the **published** crate: a version
/// requirement on the 0.9 line, and no git/path key of any kind.
#[test]
fn the_workspace_depends_on_the_published_crate() {
    let manifest = parse_toml("Cargo.toml");
    let dependency = manifest
        .get("workspace")
        .and_then(|w| w.get("dependencies"))
        .and_then(|d| d.get(CRATE))
        .and_then(|d| d.as_table())
        .unwrap_or_else(|| panic!("the workspace must manage the {CRATE} dependency"));

    for key in ["git", "branch", "rev", "path"] {
        assert!(
            !dependency.contains_key(key),
            "the {CRATE} dependency must not carry a {key:?} key — \
             it is consumed from crates.io, not from the {FORK} fork: {dependency:?}"
        );
    }

    let version = dependency
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("the {CRATE} dependency must carry a version requirement"));
    assert!(
        version.starts_with(VERSION_LINE),
        "the {CRATE} dependency must ask for the {VERSION_LINE} line, got {version:?}"
    );
}

/// The `pingora-http` dependency is gone.
///
/// Nothing in the workspace declares or imports it: the request and
/// response header types it hosts are re-exported by `pingora` and
/// `pingora-core` themselves.  Leaving the entry would pin a crate
/// line the project does not use.
#[test]
fn the_unused_pingora_http_dependency_is_gone() {
    let manifest = parse_toml("Cargo.toml");
    let workspace_dependencies = manifest
        .get("workspace")
        .and_then(|w| w.get("dependencies"))
        .and_then(|d| d.as_table())
        .expect("Cargo.toml has a [workspace.dependencies] table");
    assert!(
        !workspace_dependencies.contains_key("pingora-http"),
        "the workspace must not declare pingora-http — nothing uses it"
    );

    for member in [
        "crates/pangolin-core/Cargo.toml",
        "crates/ngx/Cargo.toml",
        "crates/tun/Cargo.toml",
        "crates/admin/Cargo.toml",
        "crates/api/Cargo.toml",
        "tests/Cargo.toml",
    ] {
        let path = repo_root().join(member);
        if !Path::new(&path).exists() {
            continue;
        }
        let member_manifest = parse_toml(member);
        for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
            let declared = member_manifest
                .get(section)
                .and_then(|d| d.as_table())
                .and_then(|d| d.get("pingora-http"));
            assert!(
                declared.is_none(),
                "{member} must not declare pingora-http — nothing uses it"
            );
        }
    }
}

/// The gateway names the two crates it uses directly, both on the
/// 0.9 line.
///
/// `crates/ngx` reaches for `pingora-core` (the HTTP client and the
/// protocol types) and `pingora-openssl` (the `ext` helpers) on its
/// own; the versions have to match the `pingora` requirement, or the
/// tree carries two copies of the same crate.
#[test]
fn the_gateway_declares_the_matching_pingora_crates() {
    let manifest = parse_toml("crates/ngx/Cargo.toml");
    let dependencies = manifest
        .get("dependencies")
        .and_then(|d| d.as_table())
        .expect("crates/ngx/Cargo.toml has a [dependencies] table");

    for crate_name in ["pingora-core", "pingora-openssl"] {
        let declared = dependencies
            .get(crate_name)
            .unwrap_or_else(|| panic!("crates/ngx must depend on {crate_name}"));

        // A dependency is either the bare version string
        // (`pingora-core = "0.9"`) or a table
        // (`pingora-core = { version = "0.9", … }`).
        let (version, table) = match declared {
            toml::Value::String(version) => (version.as_str(), None),
            toml::Value::Table(table) => (
                table
                    .get("version")
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| {
                        panic!(
                            "the {crate_name} dependency must carry a version requirement: {table:?}"
                        )
                    }),
                Some(table),
            ),
            other => panic!("the {crate_name} dependency has an unexpected shape: {other:?}"),
        };

        assert!(
            version.starts_with(VERSION_LINE),
            "the {crate_name} dependency must ask for the {VERSION_LINE} line, got {version:?}"
        );
        if let Some(table) = table {
            for key in ["git", "branch", "rev", "path"] {
                assert!(
                    !table.contains_key(key),
                    "the {crate_name} dependency must not carry a {key:?} key — \
                     it is consumed from crates.io, not from the {FORK} fork: {table:?}"
                );
            }
        }
    }
}

/// Every pingora crate in the lockfile comes from crates.io, on the
/// 0.9 line.
///
/// One entry per crate: the old tree carried **two** copies of
/// `pingora-openssl` (a git copy from the fork plus a crates.io
/// copy), because the fork's version did not satisfy the
/// requirement and cargo resolved the second one from the registry.
#[test]
fn the_lockfile_resolves_pingora_from_the_registry() {
    let packages = lock_packages();
    let pingora = pingora_packages(&packages);

    assert!(
        !pingora.is_empty(),
        "the lockfile must contain the {CRATE} crates"
    );

    for entry in &pingora {
        let name = entry.get("name").and_then(|n| n.as_str()).unwrap_or("?");
        let source = entry
            .get("source")
            .and_then(|s| s.as_str())
            .unwrap_or_else(|| panic!("{name} must come from a registry, not from a path"));
        assert!(
            source.contains("crates.io-index"),
            "{name} must resolve from crates.io, got {source:?}"
        );
        assert!(
            !source.contains(FORK),
            "{name} must not resolve from the {FORK} fork, got {source:?}"
        );

        let version = entry
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("{name} must carry a version"));
        assert!(
            version.starts_with(VERSION_LINE),
            "{name} must be on the {VERSION_LINE} line, got {version:?}"
        );
    }
}

/// The fork is gone from the whole lockfile, not just from the
/// pingora entries.
#[test]
fn no_lockfile_entry_comes_from_the_fork() {
    for entry in lock_packages() {
        let name = entry.get("name").and_then(|n| n.as_str()).unwrap_or("?");
        let source = entry.get("source").and_then(|s| s.as_str()).unwrap_or("");
        assert!(
            !source.contains(FORK),
            "{name} resolves from the {FORK} fork ({source:?}) — \
             the fork is not consumed any more"
        );
    }
}

/// Exactly one copy of each crate the gateway names directly.
#[test]
fn there_is_exactly_one_copy_of_each_pingora_crate() {
    let packages = lock_packages();
    for crate_name in ["pingora-core", "pingora-openssl"] {
        let count = packages
            .iter()
            .filter(|p| p.get("name").and_then(|n| n.as_str()) == Some(crate_name))
            .count();
        assert_eq!(
            count, 1,
            "the lockfile must carry exactly one {crate_name} entry, found {count} — \
             two copies of one crate drift apart"
        );
    }
}

/// The zstd chain the pingora tree pulls in is on a version that is
/// not affected by the advisories fixed upstream.
///
/// `zstd-safe` 7.x exposes uninitialised memory when the streaming
/// decompression of a legacy frame fails (RUSTSEC-2026-0403), and
/// `zstd-sys` 2.0.x is affected by RUSTSEC-2026-0410.  Upstream
/// raised its floor to `zstd = "0.14.3"` for exactly this reason;
/// the published 0.9 release carries it.
#[test]
fn the_zstd_chain_is_past_the_advisories() {
    let packages = lock_packages();
    let floors = [
        ("zstd", "0.14.3"),
        ("zstd-safe", "8.2.0"),
        ("zstd-sys", "2.1.2"),
    ];

    for (name, floor) in floors {
        let entries: Vec<&toml::Value> = packages
            .iter()
            .filter(|p| p.get("name").and_then(|n| n.as_str()) == Some(name))
            .collect();
        assert!(!entries.is_empty(), "the lockfile must contain {name}");

        for entry in entries {
            let version = entry
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| panic!("{name} must carry a version"));
            assert!(
                version_at_least(version, floor),
                "{name} {version} is below {floor} — the advisories RUSTSEC-2026-0403 / \
                 RUSTSEC-2026-0410 are still open"
            );
        }
    }
}

/// Compare two dotted version strings numerically, component by
/// component.  The build metadata a `zstd-sys` version carries
/// (`2.1.3+zstd.1.5.7`) is ignored: it is not a version component.
fn version_at_least(version: &str, floor: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.split(['.', '-', '+'])
            .map(|part| part.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let version = parse(version);
    let floor = parse(floor);
    for (v, f) in version.iter().zip(floor.iter()) {
        if v != f {
            return v > f;
        }
    }
    version.len() >= floor.len()
}
