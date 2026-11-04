//! Regression tests for the release version derivation (issue #130).
//!
//! The bug these pin: the release workflow *asserted* that the pushed
//! tag's version equalled `[workspace.package].version` in `Cargo.toml`,
//! so every release demanded a manual version bump before the tag
//! could be pushed — and the repository had never published a release,
//! so the version in the manifest named nothing at all.  The tag is
//! the only thing that names a release, so the tag is now the single
//! source of truth: the repository carries a placeholder version and
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
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

use crate::common::{parse_toml, read_repo_file};

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

/// One workspace member: its package name, its path in the tree and
/// its parsed manifest.
struct Member {
    name: String,
    path: String,
    manifest: toml::Value,
}

/// Every workspace member, read from the root manifest.
///
/// Derived, not hardcoded: a member that pins its own version, or one
/// that is added to the workspace later, has to be caught by the
/// checks below — a list written out by hand would let both slip
/// through.
fn workspace_members() -> Vec<Member> {
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

/// The workspace version — the single line every member inherits.
fn workspace_version() -> String {
    parse_toml("Cargo.toml")
        .get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str())
        .expect("[workspace.package].version")
        .to_string()
}

/// Every dependency table of a manifest, at any depth.
///
/// A `[target.'cfg(unix)'.dependencies]` section is a dependency table
/// too, and a pinned version in one breaks the release exactly like a
/// pinned version in `[dependencies]` does.
fn dependency_tables<'a>(value: &'a toml::Value, out: &mut Vec<&'a toml::Value>) {
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

// ── the manifest ────────────────────────────────────────────────────────────

/// The workspace version must be a version cargo can parse: the
/// manifest is what every build channel reads, and the release
/// workflow rewrites this one line.
///
/// The *value* is deliberately not pinned.  The release does not
/// depend on it — the tag is the version — so asserting a particular
/// placeholder would only be a way for the tree to fail, and would
/// block a build channel that wants to carry a real version.
#[test]
fn workspace_version_is_a_version_cargo_can_parse() {
    let version = workspace_version();
    let core = version.split(['-', '+']).next().unwrap_or_default();
    let parts: Vec<&str> = core.split('.').collect();

    assert_eq!(
        parts.len(),
        3,
        "the workspace version '{version}' is not major.minor.patch"
    );
    assert!(
        parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())),
        "the workspace version '{version}' has a non-numeric component"
    );
}

/// Every member must *inherit* the version.  A member that pins its own
/// `version = "…"` would not follow the rewrite, and the release
/// build would publish binaries that disagree with the tag.
#[test]
fn every_workspace_member_inherits_the_version() {
    for member in workspace_members() {
        let package = member
            .manifest
            .get("package")
            .unwrap_or_else(|| panic!("{} has no [package] table", member.path));

        // `version.workspace = true` is a TOML *dotted key*, so the
        // parsed manifest reports it as a table with a `workspace` key.
        assert_eq!(
            package
                .get("version")
                .and_then(|v| v.get("workspace"))
                .and_then(|v| v.as_bool()),
            Some(true),
            "{} must declare `version.workspace = true` — a pinned version would not \
             follow the release workflow's rewrite",
            member.path
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
    let members = workspace_members();
    let names: BTreeSet<&str> = members.iter().map(|m| m.name.as_str()).collect();

    for member in &members {
        let mut tables = Vec::new();
        dependency_tables(&member.manifest, &mut tables);

        for table in tables {
            let Some(deps) = table.as_table() else {
                continue;
            };
            for (name, spec) in deps {
                if !names.contains(name.as_str()) {
                    continue;
                }
                let pinned = spec
                    .as_table()
                    .map(|t| t.contains_key("version"))
                    .unwrap_or(false);
                assert!(
                    !pinned,
                    "{} requires `{name}` with an explicit version — the release workflow \
                     rewrites the shared version, so the requirement would not resolve",
                    member.path
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
    let version = workspace_version();

    for member in workspace_members() {
        let entry = packages
            .iter()
            .find(|p| p.get("name").and_then(|n| n.as_str()) == Some(member.name.as_str()))
            .unwrap_or_else(|| panic!("Cargo.lock has no entry for {}", member.name));
        assert_eq!(
            entry.get("version").and_then(|v| v.as_str()),
            Some(version.as_str()),
            "Cargo.lock must record {} at the workspace version",
            member.name
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

/// The scripts of the two steps that derive the version.
fn version_steps(workflow: &str) -> String {
    format!(
        "{}\n{}",
        step_script(workflow, "Validate the tag"),
        step_script(workflow, "Set the workspace version from the tag")
    )
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
///
/// The check is scoped to the two version steps on purpose: a `jq` in
/// the release-publishing job has nothing to do with how the version
/// is derived.
#[test]
fn release_workflow_derives_the_version_instead_of_asserting_it() {
    let workflow = read_repo_file(".github/workflows/release.yml");
    let steps = version_steps(&workflow);

    assert!(
        !steps.contains("does not match the workspace version"),
        "the version steps must not compare the tag against the manifest's version — the tag \
         is the source of truth (issue #130)"
    );
    assert!(
        !steps.contains("cargo metadata"),
        "the version steps must not read the workspace version through `cargo metadata` — \
         cargo 1.96 emits no `workspace_version` field"
    );
    assert!(
        !steps.contains("jq"),
        "the version steps must not depend on jq — nothing reads the manifest's version any more"
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

    // Every `steps.<id>.outputs` reference must point at a step that
    // exists — a reference to a step that lost its `id` silently
    // expands to the empty string.
    let declared: BTreeSet<String> = workflow
        .lines()
        .filter_map(|l| l.trim().strip_prefix("id: "))
        .map(|id| id.trim().to_string())
        .collect();
    for referenced in referenced_steps(&workflow) {
        assert!(
            declared.contains(&referenced),
            "the workflow reads `steps.{referenced}.outputs`, but no step declares that id"
        );
    }
}

/// The steps whose outputs the workflow reads.
fn referenced_steps(workflow: &str) -> BTreeSet<String> {
    let mut referenced = BTreeSet::new();
    let mut rest = workflow;
    while let Some(idx) = rest.find("steps.") {
        rest = &rest[idx + "steps.".len()..];
        let end = rest
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .unwrap_or(rest.len());
        referenced.insert(rest[..end].to_string());
        rest = &rest[end..];
    }
    referenced
}

/// A tag that is not `v<semver>` is rejected before anything is
/// compiled, and a valid one is accepted — including a pre-release.
///
/// The rejected list has the tags cargo itself rejects: the shape
/// check alone would let `v1.2.3-01` through, and the version is
/// written into `Cargo.toml`, where cargo refuses to parse it.
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
        ("v1.2.3-0.3.7", true),
        ("v1.2.3+build.1", true),
        ("0.0.1", false),
        ("v0.1", false),
        ("vx.y.z", false),
        ("v1.2.3.4", false),
        // Tags cargo cannot parse: a numeric pre-release identifier
        // with a leading zero, an empty identifier segment, a leading
        // zero in the core version.
        ("v1.2.3-01", false),
        ("v1.2.3-.", false),
        ("v1.2.3-a..b", false),
        ("v1.2.3+", false),
        ("v1.2.3+a..b", false),
        ("v01.2.3", false),
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

/// The rewrite must work on the repository's own manifest.  A legal
/// reformat of the root `Cargo.toml` — a comment on the
/// `[workspace.package]` header, say — must not turn the rewrite into
/// a silent no-op that only the read-back catches.
#[test]
fn release_workflow_rewrite_works_on_the_repository_manifest() {
    let workflow = read_repo_file(".github/workflows/release.yml");
    let script = step_script(&workflow, "Set the workspace version from the tag");
    let dir = TempDir::new().expect("temp dir");
    let original = read_repo_file("Cargo.toml");
    fs::write(dir.path().join("Cargo.toml"), &original).expect("write manifest");

    let out = run_script(&script, dir.path(), &[("TAG_VERSION", "0.0.1")]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "the rewrite must succeed on the repository's own manifest, but it failed:\n{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let after = fs::read_to_string(dir.path().join("Cargo.toml")).expect("read manifest");
    assert_eq!(
        changed_lines(&original, &after),
        1,
        "the rewrite must change exactly one line of the real manifest:\n{after}"
    );
    assert!(
        after.contains("version = \"0.0.1\""),
        "the tag's version must land in [workspace.package].version:\n{after}"
    );
    // The dependency versions of the real manifest are untouched.
    for expected in [
        "pingora = { version = \"0.8\", features = [\"lb\"] }",
        "rustls = { version = \"0.23.40\", default-features = false, features = [\"ring\", \"std\"] }",
    ] {
        assert!(
            after.contains(expected),
            "the dependency `{expected}` must not be rewritten:\n{after}"
        );
    }
}

/// The rewrite replaces the version's *value*, not the whole line: a
/// comment trailing the version belongs to the manifest, not to the
/// release.
#[test]
fn release_workflow_rewrite_keeps_a_trailing_comment() {
    let workflow = read_repo_file(".github/workflows/release.yml");
    let script = step_script(&workflow, "Set the workspace version from the tag");
    let dir = TempDir::new().expect("temp dir");
    let manifest = dir.path().join("Cargo.toml");
    let original = "[workspace.package]\nversion = \"0.0.0\" # the placeholder\n";
    fs::write(&manifest, original).expect("write manifest");

    let out = run_script(&script, dir.path(), &[("TAG_VERSION", "0.0.1")]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "the rewrite must succeed, but it failed:\n{stdout}"
    );

    let after = fs::read_to_string(&manifest).expect("read manifest");
    assert_eq!(
        after, "[workspace.package]\nversion = \"0.0.1\" # the placeholder\n",
        "the rewrite must keep the comment trailing the version"
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
        docs.contains("placeholder"),
        "the documentation must call the repository's version a placeholder"
    );
    // The documentation names the version the repository actually
    // carries, so the two cannot drift apart silently.
    let version = workspace_version();
    assert!(
        docs.contains(&version),
        "the documentation must name the repository's version ({version}) as the placeholder"
    );
    for surface in ["--version", "/healthz", "User-Agent"] {
        assert!(
            docs.contains(surface),
            "the documentation must say that a release build reports the tag version in {surface}"
        );
    }
}
