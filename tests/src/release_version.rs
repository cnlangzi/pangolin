//! Regression tests for the release version derivation (issue #130).
//!
//! The bug these pin: the release workflow *asserted* that the pushed
//! tag's version equalled `[workspace.package].version` in `Cargo.toml`,
//! so every release demanded a manual version bump before the tag
//! could be pushed — and the repository had never published a release,
//! so the `0.2.0` in the manifest named nothing at all.  The tag is
//! the only thing that names a release, so the tag is now the single
//! source of truth: the repository carries the placeholder `0.0.0` and
//! the workflow writes the tag's version into the manifest before the
//! build.
//!
//! The tests are deliberately cheap and hermetic — they parse the
//! manifest and the lockfile, and drive the workflow's own shell
//! steps against a throwaway manifest.  No network, no Docker, no
//! pangolin binaries — so they run in the day-to-day
//! `cargo test --workspace` / `make lint` cycle.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

/// The six workspace members.  All of them inherit the version through
/// `version.workspace = true`, so the single manifest line is the
/// version of every binary.
const MEMBERS: [&str; 6] = [
    "admin",
    "knownbots",
    "ngx",
    "pangolin-core",
    "pangolin-integration-tests",
    "tun",
];

/// The placeholder the repository carries.  It is *not* a released
/// version — the release workflow overwrites it with the tag's.
const PLACEHOLDER: &str = "0.0.0";

/// A stand-in for the repository manifest, shaped so that the rewrite
/// is not trivial: a `[workspace.package]` table that owns the version
/// key, and a `[workspace.dependencies]` table whose `version = "…"`
/// keys a global substitution would clobber.
///
/// The dependency is written as a *multi-line* inline table on
/// purpose.  The repository's own dependencies are single-line
/// (`pingora = { version = "0.8", … }`), where the `version` key never
/// starts a line — so a substitution anchored at the start of a line
/// would miss them and the table scoping would look unnecessary.  A
/// multi-line table puts the key at the start of a line, which is
/// what makes the scoping load-bearing.
const SYNTHETIC_MANIFEST: &str = r#"# a leading comment
[workspace.package]
version = "0.0.0"
edition = "2024"
rust-version = "1.96"

[workspace.dependencies]
pingora = { version = "0.8", features = ["lb"] }
tokio = {
  version = "1",
  features = ["full"],
}
"#;

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

// ── the manifest ────────────────────────────────────────────────────────────

/// The repository's workspace version is a placeholder, not a released
/// version.  Bumping it by hand is exactly what this change removes,
/// so a version that looks released in the tree is a regression: it
/// would put the tree back into the state where the tag and the
/// manifest disagree.
#[test]
fn workspace_version_is_the_placeholder_not_a_release() {
    let manifest = parse_toml("Cargo.toml");
    let version = manifest
        .get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str())
        .expect("[workspace.package].version");

    assert_eq!(
        version, PLACEHOLDER,
        "the repository's workspace version is a placeholder — the release workflow \
         writes the pushed tag's version into it (issue #130)"
    );
}

/// The manifest of a workspace member — the integration-test crate
/// lives outside `crates/`, but it is a member all the same.
fn member_manifest(member: &str) -> toml::Value {
    if member == "pangolin-integration-tests" {
        parse_toml("tests/Cargo.toml")
    } else {
        parse_toml(&format!("crates/{member}/Cargo.toml"))
    }
}

/// Every member must *inherit* the version.  A member that pins its own
/// `version = "…"` would not follow the rewrite, and the release
/// build would publish binaries that disagree with the tag.
#[test]
fn every_workspace_member_inherits_the_version() {
    for member in MEMBERS {
        let manifest = member_manifest(member);
        let package = manifest
            .get("package")
            .unwrap_or_else(|| panic!("{member} has no [package] table"));

        // `version.workspace = true` is a TOML *dotted key*, so the
        // parsed manifest reports it as a table with a `workspace` key.
        assert_eq!(
            package
                .get("version")
                .and_then(|v| v.get("workspace"))
                .and_then(|v| v.as_bool()),
            Some(true),
            "{member} must declare `version.workspace = true` — a pinned version would not \
             follow the release workflow's rewrite"
        );
    }
}

/// No member may require a workspace member with an explicit version.
/// The release workflow rewrites the shared version to the tag's, so a
/// requirement like `pangolin-core = { path = "…", version = "0.0.0" }`
/// would stop resolving — cargo fails with "candidate versions found
/// which didn't match" before anything is compiled.
#[test]
fn no_member_requires_a_workspace_member_with_a_version() {
    for member in MEMBERS {
        let manifest = member_manifest(member);
        for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
            let Some(deps) = manifest.get(section).and_then(|d| d.as_table()) else {
                continue;
            };
            for (name, spec) in deps {
                if !MEMBERS.contains(&name.as_str()) {
                    continue;
                }
                let Some(table) = spec.as_table() else {
                    continue;
                };
                assert!(
                    table.get("version").is_none(),
                    "{member} requires `{name}` with an explicit version — the release \
                     workflow rewrites the shared version, so the requirement would not \
                     resolve"
                );
            }
        }
    }
}

/// The lockfile has to agree with the manifest, or cargo re-resolves
/// it during the build and the release touches more than the six
/// local package entries.
#[test]
fn lockfile_records_the_workspace_version_for_every_member() {
    let lock = parse_toml("Cargo.lock");
    let packages = lock
        .get("package")
        .and_then(|p| p.as_array())
        .expect("Cargo.lock has a [[package]] array");

    for member in MEMBERS {
        let entry = packages
            .iter()
            .find(|p| p.get("name").and_then(|n| n.as_str()) == Some(member))
            .unwrap_or_else(|| panic!("Cargo.lock has no entry for {member}"));
        assert_eq!(
            entry.get("version").and_then(|v| v.as_str()),
            Some(PLACEHOLDER),
            "Cargo.lock must record {member} at the workspace version"
        );
    }
}

// ── the release workflow ────────────────────────────────────────────────────

/// The lines of the step named `step_name` in a workflow file.
///
/// The workflow is YAML, but the test crate has no YAML parser — and
/// adding one would change `Cargo.lock`, which this change
/// deliberately keeps to six version lines.  The block scalars are
/// extracted by indentation instead, the same trick `cargo_mirror.rs`
/// uses on the Dockerfile.
fn step_lines<'a>(workflow: &'a str, step_name: &str) -> Vec<&'a str> {
    let lines: Vec<&str> = workflow.lines().collect();
    let name = format!("- name: {step_name}");
    let start = lines
        .iter()
        .position(|l| l.trim() == name)
        .unwrap_or_else(|| panic!("release.yml has no step named `{step_name}`"));

    let end = lines[start + 1..]
        .iter()
        .position(|l| {
            let t = l.trim_start();
            t.starts_with("- name:") || t.starts_with("- uses:")
        })
        .map(|i| start + 1 + i)
        .unwrap_or(lines.len());

    lines[start..end].to_vec()
}

/// The `run:` script of a step, verbatim — the shell body GitHub
/// Actions would execute.
fn step_script(workflow: &str, step_name: &str) -> String {
    let lines = step_lines(workflow, step_name);
    let run = lines
        .iter()
        .position(|l| l.trim_start().starts_with("run: |"))
        .unwrap_or_else(|| panic!("step `{step_name}` has no `run: |` block"));

    let indent = |line: &str| line.len() - line.trim_start().len();
    let base = indent(lines[run]);

    // The block scalar's content is indented deeper than the `run:`
    // key; the first non-empty line fixes the amount to strip.
    let content_indent = lines[run + 1..]
        .iter()
        .find(|l| !l.trim().is_empty())
        .map(|l| indent(l))
        .filter(|i| *i > base)
        .unwrap_or_else(|| panic!("step `{step_name}` has an empty `run: |` block"));

    let mut script = String::new();
    for line in &lines[run + 1..] {
        if line.trim().is_empty() {
            script.push('\n');
            continue;
        }
        if indent(line) <= base {
            break;
        }
        script.push_str(&line[content_indent..]);
        script.push('\n');
    }
    script
}

/// Run a step's script in `dir`, the way the runner would.
fn run_script(script: &str, dir: &Path, env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(script)
        .current_dir(dir)
        // The scripts are `set -euo pipefail`; an unset variable must
        // fail the script, not silently expand to the empty string.
        .env_remove("TAG")
        .env_remove("TAG_VERSION")
        .env_remove("GITHUB_OUTPUT");
    for (key, value) in env {
        cmd.env(key, value);
    }
    cmd.output().expect("run bash")
}

/// The number of lines that differ between two texts.
fn changed_lines(before: &str, after: &str) -> usize {
    let before: Vec<&str> = before.lines().collect();
    let after: Vec<&str> = after.lines().collect();
    let mut changed = before.len().abs_diff(after.len());
    for (b, a) in before.iter().zip(after.iter()) {
        if b != a {
            changed += 1;
        }
    }
    changed
}

/// The workflow must derive the version from the tag, not compare it
/// against the manifest.  The old gate — `does not match the
/// workspace version` — is the failure this whole change exists to
/// remove, and its `cargo metadata` / `jq` read was dead code on the
/// pinned toolchain (cargo 1.96 has no `workspace_version` field; its
/// top-level `version` is the metadata *format* version).
#[test]
fn release_workflow_derives_the_version_instead_of_asserting_it() {
    let workflow = read_repo_file(".github/workflows/release.yml");

    assert!(
        !workflow.contains("does not match the workspace version"),
        "the workflow must not compare the tag against the manifest's version — the tag is \
         the source of truth (issue #130)"
    );
    assert!(
        !workflow.contains("cargo metadata"),
        "the workflow must not read the workspace version through `cargo metadata` — \
         cargo 1.96 emits no `workspace_version` field"
    );
    assert!(
        !workflow.contains("jq"),
        "the workflow must not depend on jq — nothing reads the manifest's version any more"
    );

    assert!(
        workflow.contains("Set the workspace version from the tag"),
        "the workflow must write the tag's version into the manifest"
    );
}

/// The rewrite has to happen *after* the cargo cache step: the cache
/// key hashes `**/Cargo.toml`, so rewriting first would give every
/// release a unique key and force a cold build every time.
#[test]
fn release_workflow_rewrites_the_manifest_after_the_cache_step() {
    let workflow = read_repo_file(".github/workflows/release.yml");

    let order = [
        "Cache cargo registry and target",
        "Validate the tag",
        "Assert the tagged commit is on main",
        "Set the workspace version from the tag",
        "make build",
    ];

    let positions: Vec<usize> = order
        .iter()
        .map(|step| {
            workflow
                .lines()
                .position(|l| l.trim() == format!("- name: {step}"))
                .unwrap_or_else(|| panic!("release.yml has no step named `{step}`"))
        })
        .collect();

    for i in 0..order.len() - 1 {
        assert!(
            positions[i] < positions[i + 1],
            "`{}` must come before `{}` in the workflow",
            order[i],
            order[i + 1]
        );
    }

    // The cache key must still hash the manifest — that is what makes
    // the ordering above matter.
    assert!(
        workflow.contains("hashFiles('**/Cargo.lock', '**/Cargo.toml')"),
        "the cache key must keep hashing the manifest"
    );
}

/// The version step keeps its id and its two outputs: the later steps
/// read them, and a renamed output would silently publish an archive
/// named after an empty version.
#[test]
fn release_workflow_keeps_the_version_step_outputs() {
    let workflow = read_repo_file(".github/workflows/release.yml");

    let validate = step_lines(&workflow, "Validate the tag").join("\n");
    assert!(
        validate.contains("id: version"),
        "the tag-validation step must keep the id `version` — the later steps read its outputs"
    );

    assert!(
        workflow.contains("steps.version.outputs.release_version"),
        "the archive step must keep reading `steps.version.outputs.release_version`"
    );
    assert!(
        workflow.contains("steps.version.outputs.tag_version"),
        "the --version assertion must keep reading `steps.version.outputs.tag_version`"
    );

    // No step may reference an output of a step that does not exist.
    let mut referenced = BTreeSet::new();
    let mut rest = workflow.as_str();
    while let Some(idx) = rest.find("steps.") {
        rest = &rest[idx + "steps.".len()..];
        let end = rest
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .unwrap_or(rest.len());
        referenced.insert(rest[..end].to_string());
        rest = &rest[end..];
    }
    assert_eq!(
        referenced,
        BTreeSet::from(["version".to_string()]),
        "only the `version` step's outputs may be referenced"
    );
}

/// A tag that is not `v<semver>` is rejected before anything is
/// compiled, and a valid one is accepted — including a pre-release.
#[test]
fn release_workflow_validates_the_tag_shape() {
    let workflow = read_repo_file(".github/workflows/release.yml");
    let script = step_script(&workflow, "Validate the tag");
    let dir = TempDir::new().expect("temp dir");
    let output_file = dir.path().join("github_output");

    for (tag, accepted) in [
        ("v0.0.1", true),
        ("v1.2.3", true),
        ("v0.0.1-rc.1", true),
        ("0.0.1", false),
        ("v0.1", false),
        ("vx.y.z", false),
        ("v1.2.3.4", false),
    ] {
        let _ = fs::remove_file(&output_file);
        let out = run_script(
            &script,
            dir.path(),
            &[
                ("TAG", tag),
                ("GITHUB_OUTPUT", output_file.to_str().unwrap()),
            ],
        );
        let stdout = String::from_utf8_lossy(&out.stdout);

        if accepted {
            assert!(
                out.status.success(),
                "tag `{tag}` must be accepted, but the step failed:\n{stdout}"
            );
            let outputs = fs::read_to_string(&output_file).unwrap_or_default();
            assert!(
                outputs.contains(&format!("release_version={tag}\n")),
                "the step must emit `release_version={tag}`:\n{outputs}"
            );
            assert!(
                outputs.contains(&format!("tag_version={}\n", tag.trim_start_matches('v'))),
                "the step must emit the tag version without the leading `v`:\n{outputs}"
            );
        } else {
            assert!(
                !out.status.success(),
                "tag `{tag}` must be rejected, but the step succeeded:\n{stdout}"
            );
            assert!(
                stdout.contains("::error::"),
                "the rejection must carry an `::error::` annotation:\n{stdout}"
            );
        }
    }
}

/// The rewrite touches the `version` key of the `[workspace.package]`
/// table and nothing else.  A global substitution would also rewrite
/// the `version = "…"` keys under `[workspace.dependencies]` and break
/// the build.
#[test]
fn release_workflow_rewrite_only_touches_the_workspace_package_version() {
    let workflow = read_repo_file(".github/workflows/release.yml");
    let script = step_script(&workflow, "Set the workspace version from the tag");
    let dir = TempDir::new().expect("temp dir");
    let manifest = dir.path().join("Cargo.toml");
    fs::write(&manifest, SYNTHETIC_MANIFEST).expect("write manifest");

    let out = run_script(&script, dir.path(), &[("TAG_VERSION", "0.0.1")]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "the rewrite must succeed, but it failed:\n{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let after = fs::read_to_string(&manifest).expect("read manifest");
    assert!(
        after.contains("version = \"0.0.1\""),
        "the tag's version must land in [workspace.package].version:\n{after}"
    );
    assert!(
        after.contains("pingora = { version = \"0.8\", features = [\"lb\"] }")
            && after.contains("  version = \"1\",\n"),
        "the dependency versions must not be rewritten:\n{after}"
    );
    assert_eq!(
        changed_lines(SYNTHETIC_MANIFEST, &after),
        1,
        "the rewrite must change exactly one line:\n{after}"
    );
}

/// A manifest whose `[workspace.package]` has no `version` key cannot
/// be rewritten — the step has to fail loudly instead of building
/// binaries that report the wrong version.
#[test]
fn release_workflow_rewrite_fails_without_a_workspace_package_version() {
    let workflow = read_repo_file(".github/workflows/release.yml");
    let script = step_script(&workflow, "Set the workspace version from the tag");
    let dir = TempDir::new().expect("temp dir");
    let manifest = dir.path().join("Cargo.toml");
    fs::write(&manifest, "[workspace.package]\nedition = \"2024\"\n").expect("write manifest");

    let out = run_script(&script, dir.path(), &[("TAG_VERSION", "0.0.1")]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !out.status.success(),
        "the step must fail when there is no version to write:\n{stdout}"
    );
    assert!(
        stdout.contains("::error::"),
        "the failure must carry an `::error::` annotation:\n{stdout}"
    );
    assert!(
        stdout.contains("Cargo.toml")
            && stdout.contains("[workspace.package]")
            && stdout.contains("0.0.1"),
        "the error must name Cargo.toml, [workspace.package] and the expected version:\n{stdout}"
    );
}

// ── the documentation ───────────────────────────────────────────────────────

/// The release procedure is part of the deliverable: the operator
/// must not be told to bump a version any more.
#[test]
fn release_docs_describe_the_tag_as_the_version() {
    let docs = read_repo_file("docs/release.md");

    assert!(
        !docs.contains("Bump the version"),
        "the procedure must not ask the operator to bump the version — the tag is the version"
    );
    assert!(
        docs.contains(PLACEHOLDER),
        "the documentation must state that the repository carries the {PLACEHOLDER} placeholder"
    );
    assert!(
        docs.contains("placeholder"),
        "the documentation must call the repository's version a placeholder"
    );
    for surface in ["--version", "/healthz", "User-Agent"] {
        assert!(
            docs.contains(surface),
            "the documentation must say that a release build reports the tag version in {surface}"
        );
    }
}
