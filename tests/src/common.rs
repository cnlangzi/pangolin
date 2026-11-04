//! Shared helpers for the hermetic build-system tests.
//!
//! `cargo_mirror` (issue #126) and `release_version` (issue #130)
//! both read files out of the repository root and parse TOML.  The
//! helpers live here so a change to the panic formatting or to an
//! error message cannot leave one module silently stale.

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
