//! Shared helpers for the hermetic build-system tests.
//!
//! `cargo_mirror` (issue #126), `release_version` (issue #130) and
//! the dependency-swap tests (issues #146, #159, #189) all read
//! files out of the repository root and parse TOML.  The helpers
//! live here so a change to the panic formatting or to an error
//! message cannot leave one module silently stale — and so a member
//! list is derived from the manifest instead of written out by hand
//! in three places.

use std::fs;
use std::path::{Path, PathBuf};

/// The repository root — the parent of the `tests/` crate.
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tests/ lives directly under the repo root")
        .to_path_buf()
}

/// A file of the repository, as text.
pub fn read_repo_file(relative: &str) -> String {
    let path = repo_root().join(relative);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// A file of the repository, parsed as TOML.
pub fn parse_toml(relative: &str) -> toml::Value {
    let path = repo_root().join(relative);
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    toml::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

/// The `[[package]]` array of the lockfile.
pub fn lock_packages() -> Vec<toml::Value> {
    parse_toml("Cargo.lock")
        .get("package")
        .and_then(|p| p.as_array())
        .expect("Cargo.lock has a [[package]] array")
        .clone()
}

/// The `[[package]]` entry of the lockfile for `name`.
pub fn lock_entry<'a>(packages: &'a [toml::Value], name: &str) -> &'a toml::Value {
    packages
        .iter()
        .find(|p| p.get("name").and_then(|n| n.as_str()) == Some(name))
        .unwrap_or_else(|| panic!("Cargo.lock has no entry for {name}"))
}

/// One workspace member: its package name, its path in the tree and
/// its parsed manifest.
pub struct Member {
    pub name: String,
    pub path: String,
    pub manifest: toml::Value,
}

/// Every workspace member, read from the root manifest.
///
/// Derived, not hardcoded: a member that pins its own version, or one
/// that is added to the workspace later, has to be caught by the
/// checks below — a list written out by hand would let both slip
/// through.
pub fn workspace_members() -> Vec<Member> {
    let manifest = parse_toml("Cargo.toml");
    let members = manifest
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
        .expect("[workspace] members");

    members
        .iter()
        .map(|path| {
            let path = path.as_str().expect("a member path");
            let member = parse_toml(&format!("{path}/Cargo.toml"));
            let name = member
                .get("package")
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or_else(|| panic!("{path} declares no package name"))
                .to_string();
            Member {
                name,
                path: path.to_string(),
                manifest: member,
            }
        })
        .collect()
}

/// Every dependency table of a manifest, at any depth.
///
/// A `[target.'cfg(unix)'.dependencies]` section is a dependency table
/// too, and a pinned version in one breaks the release exactly like a
/// pinned version in `[dependencies]` does.
pub fn dependency_tables<'a>(value: &'a toml::Value, out: &mut Vec<&'a toml::Value>) {
    let Some(table) = value.as_table() else {
        return;
    };
    for (key, child) in table {
        if matches!(
            key.as_str(),
            "dependencies" | "dev-dependencies" | "build-dependencies"
        ) {
            out.push(child);
        }
        dependency_tables(child, out);
    }
}
