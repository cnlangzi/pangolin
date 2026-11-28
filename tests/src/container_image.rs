//! Regression tests for the runtime container images (issue #275).
//!
//! A release publishes two container images next to the archives and
//! the packages:
//!
//!   * `ghcr.io/cnlangzi/pangolin-ngx` — the gateway
//!   * `ghcr.io/cnlangzi/pangolin-tun` — the tunnel node
//!
//! Both are **assembled** from the release binaries, never compiled
//! inside a container, and both carry the *packaged* configuration
//! templates — the ones the deb / rpm packages install — so that the
//! paths in the configuration mean the same thing in a container as
//! they do on a host.
//!
//! The tests below pin the contract that is easy to break and
//! expensive to discover late:
//!
//!   * the release workflow has the two image jobs, and the GitHub
//!     Release does **not** wait for them — a registry that is down
//!     must not keep the operators from getting the binaries;
//!   * the images are pushed by digest and merged into a manifest
//!     list, so a single architecture never occupies `v<version>`;
//!   * the image carries the release binary and no compiler;
//!   * the directories the image creates are exactly the directories
//!     the packages own;
//!   * the configuration in the image is the packaged one, with the
//!     privileged ports and the absolute paths.
//!
//! The tests need neither the `integration` feature, nor a network,
//! nor root, nor Docker: they read the workflow and the Dockerfile,
//! and drive the staging script with throwaway binaries.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::common::{read_repo_file, repo_root};
use crate::release_version::step_script;

/// The two images a release publishes.
const IMAGES: [&str; 2] = ["pangolin-ngx", "pangolin-tun"];

/// The two architectures every image covers.
const PLATFORMS: [&str; 2] = ["linux/amd64", "linux/arm64"];

/// The registry the images live in.
const REGISTRY: &str = "ghcr.io";

/// The namespace under the registry.
const OWNER: &str = "cnlangzi";

/// The base image of both runtime images.
const BASE_IMAGE: &str = "debian:bookworm-slim";

// ── helpers ─────────────────────────────────────────────────────────────────

/// A tool of the host, found the way the shell would find it.
fn host_tool(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// The body of one job of the release workflow.
///
/// The jobs are separated by a line indented with exactly two spaces;
/// everything up to the next such line is the job's own block.  The
/// indentation matters: a job's comments are indented deeper, and a
/// search for "two spaces" alone would cut the block at the first
/// comment.
fn job(workflow: &str, name: &str) -> String {
    let lines: Vec<&str> = workflow.lines().collect();
    let start = lines
        .iter()
        .position(|line| line.starts_with(&format!("  {name}:")))
        .unwrap_or_else(|| panic!("the release workflow has no `{name}` job"));
    let end = lines[start + 1..]
        .iter()
        .position(|line| line.starts_with("  ") && !line.starts_with("   "))
        .map(|offset| start + 1 + offset)
        .unwrap_or(lines.len());
    lines[start..end].join("\n")
}

/// One stage of the runtime Dockerfile.
struct Stage {
    name: String,
    body: String,
}

/// The stages of the runtime Dockerfile, in the order they appear.
fn stages(text: &str) -> Vec<Stage> {
    let mut out: Vec<Stage> = Vec::new();
    let mut current: Option<(String, String)> = None;

    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("FROM ") {
            if let Some((name, body)) = current.take() {
                out.push(Stage { name, body });
            }
            let name = rest
                .rsplit(" AS ")
                .next()
                .expect("every FROM of the runtime Dockerfile names a stage")
                .trim()
                .to_string();
            // The `FROM` line belongs to the stage it opens: it names
            // the base image the stage is built on.
            current = Some((name, format!("{trimmed}\n")));
            continue;
        }
        if let Some((_, body)) = current.as_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    if let Some((name, body)) = current.take() {
        out.push(Stage { name, body });
    }
    out
}

/// The instructions of a stage, one logical line each.
///
/// A Dockerfile instruction may be spread over several lines with a
/// trailing backslash; the instructions are read the way Docker reads
/// them, so that a value split over two lines is one value.
fn instructions(stage: &Stage) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();

    for line in stage.body.lines() {
        let trimmed = line.trim();
        if current.is_empty() {
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            current = trimmed.to_string();
        } else {
            current.push(' ');
            current.push_str(trimmed);
        }
        if current.ends_with('\\') {
            current.pop();
            continue;
        }
        out.push(std::mem::take(&mut current));
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// The value of one instruction of a stage, e.g. the `WORKDIR`.
fn instruction(stage: &Stage, name: &str) -> String {
    for line in instructions(stage) {
        if let Some(rest) = line.strip_prefix(name)
            && rest.starts_with(' ')
        {
            return rest.trim().to_string();
        }
    }
    panic!("the {} stage has no {name} instruction", stage.name);
}

/// The `needs:` list of a job.
fn needs(job: &str) -> Vec<String> {
    for line in job.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("needs:") {
            return rest
                .trim()
                .trim_start_matches('[')
                .trim_end_matches(']')
                .split(',')
                .map(|item| item.trim().to_string())
                .filter(|item| !item.is_empty())
                .collect();
        }
    }
    Vec::new()
}

/// The quoted elements of a Dockerfile's JSON array.
fn json_array(value: &str) -> Vec<String> {
    let trimmed = value.trim();
    assert!(
        trimmed.starts_with('[') && trimmed.ends_with(']'),
        "not a JSON array: {value}"
    );
    trimmed[1..trimmed.len() - 1]
        .split(',')
        .map(|part| part.trim().trim_matches('"').to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

/// The directories a stage creates with `mkdir -p`.
fn created_directories(stage: &Stage) -> BTreeSet<String> {
    let mut dirs = BTreeSet::new();
    for line in instructions(stage) {
        let Some(rest) = line.strip_prefix("RUN ") else {
            continue;
        };
        for part in rest.split("&&") {
            let Some(paths) = part.trim().strip_prefix("mkdir -p ") else {
                continue;
            };
            for path in paths.split_whitespace() {
                dirs.insert(path.to_string());
            }
        }
    }
    dirs
}

/// The directories a package declares in its file list.
///
/// The file list is a YAML list whose entries are flat: every
/// `- type: dir` block is followed by a `dst:` line.  Reading the two
/// together keeps the test free of the `@@…@@` placeholders the
/// nfpm renderer substitutes.
fn package_directories(relative: &str) -> BTreeSet<String> {
    let text = read_repo_file(relative);
    let mut dirs = BTreeSet::new();
    let mut is_dir = false;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "- type: dir" {
            is_dir = true;
            continue;
        }
        if trimmed.starts_with("- ") {
            is_dir = false;
            continue;
        }
        if is_dir && let Some(dst) = trimmed.strip_prefix("dst: ") {
            dirs.insert(dst.trim().to_string());
            is_dir = false;
        }
    }
    dirs
}

/// The runtime Dockerfile.
fn dockerfile() -> String {
    read_repo_file("packaging/container/runtime.dockerfile")
}

/// The stage of the runtime Dockerfile with the given name.
fn stage(name: &str) -> Stage {
    stages(&dockerfile())
        .into_iter()
        .find(|stage| stage.name == name)
        .unwrap_or_else(|| panic!("the runtime Dockerfile has no `{name}` stage"))
}

/// The tags the manifest job creates, read out of its `imagetools
/// create` invocation.
fn created_tags(job: &str) -> Vec<String> {
    let mut tags = Vec::new();
    for line in job.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("--tag ") {
            let value = rest.trim().trim_end_matches('\\').trim();
            tags.push(value.trim_matches('"').to_string());
        }
    }
    tags
}

// ── the workflow ────────────────────────────────────────────────────────────

/// The release workflow publishes the two container images.
///
/// The `docker` job builds one architecture per runner and pushes it
/// by digest; the `docker-manifest` job merges the digests into the
/// tagged manifest lists.  Neither may be missing: an image that is
/// never built is an image nobody can pull.
#[test]
fn the_release_workflow_has_the_docker_and_docker_manifest_jobs() {
    let workflow = read_repo_file(".github/workflows/release.yml");

    let docker = job(&workflow, "docker");
    assert_eq!(
        needs(&docker),
        vec!["build".to_string()],
        "the docker job must build on the release binaries"
    );

    let manifest = job(&workflow, "docker-manifest");
    assert_eq!(
        needs(&manifest),
        vec!["docker".to_string()],
        "the docker-manifest job must wait for the per-architecture images"
    );
}

/// The GitHub Release does not wait for the container images.
///
/// A registry that is down, or a push that the token is not allowed
/// to do, must not keep the operators from getting the binaries and
/// the packages.  The image jobs fail the *workflow*, not the release.
#[test]
fn the_release_job_does_not_wait_for_the_container_images() {
    let workflow = read_repo_file(".github/workflows/release.yml");

    let release = job(&workflow, "release");
    assert_eq!(
        needs(&release),
        vec![
            "build".to_string(),
            "build-mac".to_string(),
            "build-tun".to_string(),
            "package".to_string(),
        ],
        "the release job's needs must not change: {release}"
    );
    for image_job in ["docker", "docker-manifest"] {
        assert!(
            !needs(&release).contains(&image_job.to_string()),
            "the release job must not depend on the {image_job} job"
        );
    }
}

/// The image jobs may write to the registry, and nothing else.
///
/// GHCR is written with the workflow's own token, which needs
/// `packages: write`.  The workflow's top-level `contents: read`
/// stays: an image job pushes images, not commits.
#[test]
fn the_container_jobs_may_write_to_the_registry() {
    let workflow = read_repo_file(".github/workflows/release.yml");

    assert!(
        workflow.contains("permissions:\n  contents: read\n"),
        "the workflow must stay read-only by default"
    );
    for image_job in ["docker", "docker-manifest"] {
        let block = job(&workflow, image_job);
        assert!(
            block.contains("packages: write"),
            "the {image_job} job needs `packages: write` to push to GHCR"
        );
    }
}

/// The images are tagged with the release tag and with `latest`.
///
/// The tag is the tag, verbatim — the leading `v` stays, the same
/// way it does in the release tag, in the archive name and in
/// `--version`.  `latest` points at the last version that was pushed
/// successfully.
#[test]
fn the_container_images_are_tagged_with_the_release_tag_and_latest() {
    let workflow = read_repo_file(".github/workflows/release.yml");
    let manifest = job(&workflow, "docker-manifest");

    assert!(
        manifest.contains("RELEASE_VERSION: ${{ github.ref_name }}"),
        "the image tag must be the pushed tag, verbatim: {manifest}"
    );

    let tags = created_tags(&manifest);
    assert_eq!(
        tags.len(),
        2,
        "the manifest job must create exactly two tags: {tags:?}"
    );
    assert!(
        tags.iter().any(|tag| tag == "$repo:$RELEASE_VERSION"),
        "the version tag is missing: {tags:?}"
    );
    assert!(
        tags.iter().any(|tag| tag == "$repo:latest"),
        "the latest tag is missing: {tags:?}"
    );
}

/// The single-architecture images are pushed by digest and merged
/// afterwards.
///
/// A single-architecture image must never hold `v<version>`, not even
/// for a moment: a `docker pull` in that window would fetch the wrong
/// architecture.  The tags are created by the manifest job, out of
/// the digests.
#[test]
fn the_container_images_are_pushed_by_digest_and_merged_into_a_manifest() {
    let workflow = read_repo_file(".github/workflows/release.yml");

    let docker = job(&workflow, "docker");
    assert!(
        docker.contains("push-by-digest=true"),
        "the per-architecture image must be pushed by digest: {docker}"
    );

    let manifest = job(&workflow, "docker-manifest");
    assert!(
        manifest.contains("docker buildx imagetools create"),
        "the manifest job must merge the digests: {manifest}"
    );
    for slug in ["linux-amd64", "linux-arm64"] {
        assert!(
            manifest.contains(slug),
            "the manifest job must use the {slug} digest: {manifest}"
        );
    }
    for platform in PLATFORMS {
        assert!(
            manifest.contains(platform),
            "the manifest job must assert that {platform} is listed: {manifest}"
        );
    }
}

/// The `latest` tag is written under a concurrency group.
///
/// Two releases pushing at the same time would race, and the loser
/// would leave `latest` pointing at the older version.  The group
/// serializes the writes; `cancel-in-progress: false` keeps a push
/// that is already running from being cancelled.
#[test]
fn the_latest_tag_is_written_under_a_concurrency_group() {
    let workflow = read_repo_file(".github/workflows/release.yml");
    let manifest = job(&workflow, "docker-manifest");

    assert!(
        manifest.contains("group: ghcr-push"),
        "the manifest job must serialize the writes of the latest tag: {manifest}"
    );
    assert!(
        manifest.contains("cancel-in-progress: false"),
        "the manifest job must not cancel a push that is already running: {manifest}"
    );
}

/// The images are assembled from the release binaries, and the
/// workflow never compiles inside the container.
///
/// The image has to carry the bytes the GitHub Release ships.  The
/// other Docker pipeline of this repository compiles with a different
/// toolchain, so a release that compiled here would publish "another
/// binary".
#[test]
fn the_container_images_are_assembled_from_the_release_binaries() {
    let workflow = read_repo_file(".github/workflows/release.yml");
    let docker = job(&workflow, "docker");

    assert!(
        docker.contains("packaging/container/stage.sh"),
        "the docker job must assemble the build context with the staging script: {docker}"
    );
    assert!(
        docker.contains("name: pangolin-${{ matrix.slug }}"),
        "the docker job must use the binaries of the build job: {docker}"
    );
    for compiler in ["cargo build", "rustc ", "make build"] {
        assert!(
            !docker.contains(compiler),
            "the docker job must not compile in the container ({compiler}): {docker}"
        );
    }
}

/// The images carry the release binary and no toolchain.
///
/// A compiler in the image means the image was built from source
/// instead of assembled from the release artefact — and the release
/// artefact is what the GitHub Release ships.
#[test]
fn the_container_images_carry_the_release_binary_and_not_a_compiler() {
    let workflow = read_repo_file(".github/workflows/release.yml");
    let docker = job(&workflow, "docker");

    assert!(
        docker.contains("command -v cargo"),
        "the docker job must check that the image ships no compiler: {docker}"
    );
    assert!(
        docker.contains("--version"),
        "the docker job must check the version the image reports: {docker}"
    );
}

/// Every operator-facing document names the container images.
///
/// A distribution shape that only one document mentions is a shape
/// nobody finds.  The list is the set of documents the release
/// touches; a new one enters it here.
#[test]
fn the_operator_facing_documents_name_the_container_images() {
    const DOCUMENTS: [&str; 8] = [
        "docs/release.md",
        "docs/development.md",
        "docs/getting-started.md",
        "docs/getting-started.en-US.md",
        "docs/packaging.md",
        "docs/packaging.en-US.md",
        "README.md",
        "README.en-US.md",
    ];

    for document in DOCUMENTS {
        let text = read_repo_file(document);
        assert!(
            text.contains(&format!("docker pull {REGISTRY}/{OWNER}/pangolin-ngx")),
            "{document} must tell the operator how to pull the gateway image"
        );
        for image in IMAGES {
            assert!(
                text.contains(&format!("{REGISTRY}/{OWNER}/{image}")),
                "{document} must name the {image} image"
            );
        }
    }
}

/// The base image is a Debian with a CA bundle.
///
/// The gateway talks HTTPS to the ACME directory and to the
/// dashboard, and both need a CA bundle; the Debian slim image does
/// not ship one.
#[test]
fn the_base_image_is_a_debian_with_a_ca_bundle() {
    let base = stage("base");
    let from = instruction(&base, "FROM");

    assert!(
        from.starts_with(BASE_IMAGE),
        "the base image must be {BASE_IMAGE}: {from}"
    );
    assert!(
        instruction(&base, "RUN").contains("ca-certificates"),
        "the base image must install the CA bundle: {}",
        instruction(&base, "RUN")
    );
}

/// The glibc floor is asserted before anything is pushed.
///
/// The base image's glibc has to be at least as new as the highest
/// `GLIBC_x.y` symbol version the binaries record.  The floor is
/// *measured*, never written down — the same rule the packages
/// follow.
#[test]
fn the_glibc_floor_is_asserted_before_the_push() {
    let workflow = read_repo_file(".github/workflows/release.yml");
    let docker = job(&workflow, "docker");

    assert!(
        docker.contains("objdump -T"),
        "the docker job must measure the glibc floor of the binaries: {docker}"
    );
    assert!(
        docker.contains("GNU_LIBC_VERSION"),
        "the docker job must measure the glibc version of the base image: {docker}"
    );

    let measure = docker
        .find("objdump -T")
        .expect("the glibc measurement is there");
    let push = docker
        .find("docker buildx build")
        .expect("the push is there");
    assert!(
        measure < push,
        "the glibc floor must be asserted before anything is pushed"
    );
}

/// The glibc floor is compared in the right direction.
///
/// The base image has to be at least as new as the floor the binaries
/// record.  Getting the two the wrong way round is invisible in the
/// happy path's *numbers* and fatal in practice: the release
/// binaries are built on ubuntu-22.04 (a glibc 2.35 floor) and the
/// base image is bookworm (glibc 2.36), so an inverted comparison
/// fails every release — while a base image that really is too old
/// sails through.
///
/// The workflow's own step is driven here, with a stubbed `objdump`
/// and a stubbed `docker`, in both directions: the comparison and
/// the error message are the workflow's own shell.
#[test]
fn the_glibc_floor_is_compared_in_the_right_direction() {
    let workflow = read_repo_file(".github/workflows/release.yml");
    let script = step_script(
        &workflow,
        "Assert the base image is new enough for the binaries",
    );

    // The happy path: the binaries need 2.35 and the base image
    // ships 2.36.  The base image is new enough, so the step passes.
    let ok = run_glibc_step(&script, "2.35", "2.36");
    assert!(
        ok.status.success(),
        "a base image newer than the floor must pass, but the step failed: {}",
        String::from_utf8_lossy(&ok.stderr)
    );

    // The broken case: the binaries need 2.36 and the base image
    // ships 2.35.  The step must fail, and the message has to name
    // both versions — the operator has to know which one to move.
    let bad = run_glibc_step(&script, "2.36", "2.35");
    assert!(
        !bad.status.success(),
        "a base image older than the floor must fail the step"
    );
    // The workflow command goes to stdout, like every other `::error::`
    // in this workflow.
    let mut message = String::from_utf8_lossy(&bad.stdout).into_owned();
    message.push_str(&String::from_utf8_lossy(&bad.stderr));
    for version in ["2.36", "2.35"] {
        assert!(
            message.contains(version),
            "the failure message must name the version {version}: {message}"
        );
    }
}

/// Run the workflow's glibc step with a stubbed `objdump` and
/// `docker`.
///
/// The stubs answer the two questions the step asks — the glibc
/// version the binary records, and the glibc version of the base
/// image — and nothing else: the measurement, the comparison and the
/// error message are the workflow's own shell.
fn run_glibc_step(script: &str, binary_glibc: &str, base_glibc: &str) -> Output {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let work = dir.path().join("work");
    let bin = work.join("bin");
    fs::create_dir_all(&bin).expect("the binary directory");
    for name in IMAGES {
        fs::write(bin.join(name), b"not a real binary").expect("a stand-in binary");
    }

    // The stubs come first on PATH, so the step finds them instead
    // of the host's tools.
    let stub_dir = work.join("stub");
    fs::create_dir_all(&stub_dir).expect("the stub directory");
    write_stub(
        &stub_dir.join("objdump"),
        &format!("#!/bin/sh\necho \"  GLIBC_{binary_glibc}  version\"\n"),
    );
    write_stub(
        &stub_dir.join("docker"),
        &format!("#!/bin/sh\necho \"glibc {base_glibc}\"\n"),
    );

    let path = format!(
        "{}:{}",
        stub_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    Command::new(host_tool("bash").expect("bash"))
        .arg("-c")
        .arg(script)
        .current_dir(&work)
        .env("PATH", path)
        .env("BASE_IMAGE", "debian:bookworm-slim")
        .output()
        .expect("run the glibc step")
}

/// Write an executable stub script.
fn write_stub(path: &Path, body: &str) {
    fs::write(path, body).expect("the stub");
    let mut permissions = fs::metadata(path).expect("the stub").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("make the stub executable");
}

/// The GHCR packages are made public, and a failure to do so only
/// warns.
///
/// A GHCR package is private by default, so an anonymous `docker
/// pull` of a freshly published image would be rejected.  The
/// workflow tries to flip the package to public and only warns when
/// it cannot: a permission problem must not turn the whole release
/// red, and the images are pushed either way.
#[test]
fn the_ghcr_packages_are_made_public_with_a_warning_only() {
    let workflow = read_repo_file(".github/workflows/release.yml");
    let manifest = job(&workflow, "docker-manifest");

    assert!(
        manifest.contains("\"visibility\":\"public\""),
        "the manifest job must ask for the packages to become public: {manifest}"
    );
    assert!(
        manifest.contains("::warning::"),
        "a package that stays private must only produce a warning: {manifest}"
    );
}

/// The GitHub Release names the two container images.
///
/// The Release page is the one place that tells the operator every
/// shape of a release, so it has to name the images and their tags.
#[test]
fn the_release_body_names_the_two_container_images() {
    let workflow = read_repo_file(".github/workflows/release.yml");
    let release = job(&workflow, "release");

    for image in IMAGES {
        let reference = format!("{REGISTRY}/{OWNER}/{image}");
        assert!(
            release.contains(&reference),
            "the release body must name {reference}: {release}"
        );
    }
}

// ── the runtime Dockerfile ──────────────────────────────────────────────────

/// The Dockerfile has the three stages the images are built from.
#[test]
fn the_runtime_dockerfile_has_the_base_ngx_and_tun_stages() {
    let names: Vec<String> = stages(&dockerfile())
        .into_iter()
        .map(|stage| stage.name)
        .collect();

    assert_eq!(names, vec!["base", "ngx", "tun"]);
}

/// The gateway image runs the gateway binary with the packaged
/// configuration.
#[test]
fn the_gateway_image_declares_its_entrypoint_command_and_working_directory() {
    let ngx = stage("ngx");

    assert_eq!(
        json_array(&instruction(&ngx, "ENTRYPOINT")),
        vec!["/usr/bin/pangolin-ngx".to_string()]
    );
    assert_eq!(
        json_array(&instruction(&ngx, "CMD")),
        vec!["--config".to_string(), "/etc/pangolin/ngx.yml".to_string()]
    );
    assert_eq!(instruction(&ngx, "WORKDIR"), "/var/lib/pangolin");
}

/// The tunnel node image runs the tunnel binary with the packaged
/// configuration.
#[test]
fn the_tunnel_image_declares_its_entrypoint_command_and_working_directory() {
    let tun = stage("tun");

    assert_eq!(
        json_array(&instruction(&tun, "ENTRYPOINT")),
        vec!["/usr/bin/pangolin-tun".to_string()]
    );
    assert_eq!(
        json_array(&instruction(&tun, "CMD")),
        vec!["--config".to_string(), "/etc/pangolin/tun.yml".to_string()]
    );
    assert_eq!(instruction(&tun, "WORKDIR"), "/var/lib/pangolin-tun");
}

/// The images declare the ports they listen on.
///
/// The tunnel endpoint is 9001 twice: QUIC over UDP and the TCP
/// fallback over TCP.
#[test]
fn the_images_declare_the_ports_they_listen_on() {
    let ngx = stage("ngx");
    let exposed: Vec<String> = instruction(&ngx, "EXPOSE")
        .split_whitespace()
        .map(str::to_string)
        .collect();
    for port in ["80/tcp", "443/tcp", "9081/tcp", "9001/udp", "9001/tcp"] {
        assert!(
            exposed.iter().any(|declared| declared == port),
            "the gateway image must expose {port}: {exposed:?}"
        );
    }

    let tun = stage("tun");
    let exposed: Vec<String> = instruction(&tun, "EXPOSE")
        .split_whitespace()
        .map(str::to_string)
        .collect();
    for port in ["9001/udp", "9001/tcp"] {
        assert!(
            exposed.iter().any(|declared| declared == port),
            "the tunnel image must expose {port}: {exposed:?}"
        );
    }
}

/// The images declare their state directories as volumes.
///
/// Without them, removing the container takes the database, the
/// certificates and the logs with it.
#[test]
fn the_images_declare_their_state_directories_as_volumes() {
    let ngx = stage("ngx");
    let volumes = json_array(&instruction(&ngx, "VOLUME"));
    for volume in [
        "/var/lib/pangolin",
        "/var/log/pangolin",
        "/var/cache/pangolin",
    ] {
        assert!(
            volumes.iter().any(|declared| declared == volume),
            "the gateway image must declare {volume} a volume: {volumes:?}"
        );
    }

    let tun = stage("tun");
    let volumes = json_array(&instruction(&tun, "VOLUME"));
    assert!(
        volumes
            .iter()
            .any(|declared| declared == "/var/lib/pangolin-tun"),
        "the tunnel image must declare /var/lib/pangolin-tun a volume: {volumes:?}"
    );
}

/// The images carry the OCI labels that name their source.
///
/// The label's version is the tag, verbatim, and the revision is the
/// commit the tag points at.
#[test]
fn the_images_carry_the_oci_labels() {
    for name in ["ngx", "tun"] {
        let labels = instruction(&stage(name), "LABEL");
        assert!(
            labels.contains(
                "org.opencontainers.image.source=\"https://github.com/cnlangzi/pangolin\""
            ),
            "the {name} image must name its source: {labels}"
        );
        assert!(
            labels.contains("org.opencontainers.image.version="),
            "the {name} image must carry its version: {labels}"
        );
        assert!(
            labels.contains("org.opencontainers.image.revision="),
            "the {name} image must carry its revision: {labels}"
        );
    }
}

/// The image directories are the package directories.
///
/// An operator who migrates from the deb package to the container
/// must not have to change a single path in the configuration, and the
/// directories the package owns are the ones the image creates.  The
/// volume declarations cover the *parents* of the log and cache
/// directories — Docker copies a volume's content out of the image
/// when it creates the volume, so the subdirectories have to exist in
/// the image or a fresh volume would come up empty.
#[test]
fn the_image_directories_match_the_package_directories() {
    assert_eq!(
        created_directories(&stage("ngx")),
        package_directories("packaging/contents/pangolin-ngx.yaml"),
        "the gateway image must create exactly the directories the package owns"
    );
    assert_eq!(
        created_directories(&stage("tun")),
        package_directories("packaging/contents/pangolin-tun.yaml"),
        "the tunnel image must create exactly the directories the package owns"
    );
}

/// The image carries the *packaged* configuration template.
///
/// The release archive carries the repository's development
/// configuration — relative paths, the ports 8000 and 8443 — and an
/// image that shipped it would come up on the wrong ports, with paths
/// that follow the working directory.
#[test]
fn the_image_carries_the_packaged_configuration_templates() {
    let ngx = read_repo_file("packaging/config/ngx.yml");
    assert!(
        ngx.contains("http: 0.0.0.0:80"),
        "the packaged gateway configuration binds the privileged port"
    );
    // The development port may be *named* in a comment — the template
    // says what it is not — but no setting may carry it.
    let settings: Vec<&str> = ngx
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    assert!(
        !settings.iter().any(|line| line.contains("8000")),
        "the packaged gateway configuration must not carry the development port: {settings:?}"
    );

    let tun = read_repo_file("packaging/config/tun.yml");
    assert!(
        tun.contains("pin_path: /var/lib/pangolin-tun/tun.pins"),
        "the packaged tunnel configuration names an absolute pin path"
    );

    // The image copies the packaged template, not the archive's copy.
    let dockerfile = dockerfile();
    assert!(
        dockerfile.contains("COPY ngx.yml /etc/pangolin/ngx.yml"),
        "the gateway image must copy the staged ngx.yml"
    );
    assert!(
        dockerfile.contains("COPY tun.yml /etc/pangolin/tun.yml"),
        "the tunnel image must copy the staged tun.yml"
    );
}

// ── the staging script ──────────────────────────────────────────────────────

/// The staging script produces exactly the build context.
///
/// Four files: the two binaries and the two packaged configuration
/// templates, with the modes the package gives them.  Anything else
/// in the context would be baked into the image.
#[test]
fn the_staging_script_produces_exactly_the_build_context() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let bin_dir = dir.path().join("bin");
    fs::create_dir_all(&bin_dir).expect("the binary directory");
    for name in IMAGES {
        fs::write(bin_dir.join(name), b"#!/bin/sh\nexit 0\n").expect("a stand-in binary");
    }
    let context = dir.path().join("context");

    run_staging_script(&bin_dir, &context);

    let mut entries: Vec<String> = fs::read_dir(&context)
        .expect("the build context")
        .map(|entry| {
            entry
                .expect("a context entry")
                .file_name()
                .to_string_lossy()
                .into()
        })
        .collect();
    entries.sort();
    assert_eq!(
        entries,
        vec![
            "ngx.yml".to_string(),
            "pangolin-ngx".to_string(),
            "pangolin-tun".to_string(),
            "tun.yml".to_string(),
        ]
    );

    for (name, mode) in [
        ("pangolin-ngx", 0o755),
        ("pangolin-tun", 0o755),
        ("ngx.yml", 0o640),
        ("tun.yml", 0o640),
    ] {
        let path = context.join(name);
        let permissions = fs::metadata(&path).expect("the staged file").permissions();
        assert_eq!(
            permissions.mode() & 0o777,
            mode,
            "{name} must be staged with mode {mode:o}"
        );
    }
}

/// The staged configuration is the packaged one, byte for byte.
///
/// The image and the deb / rpm package have to install the *same*
/// configuration — one fact, one place.
#[test]
fn the_staged_configuration_is_the_packaged_one() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let bin_dir = dir.path().join("bin");
    fs::create_dir_all(&bin_dir).expect("the binary directory");
    for name in IMAGES {
        fs::write(bin_dir.join(name), b"#!/bin/sh\nexit 0\n").expect("a stand-in binary");
    }
    let context = dir.path().join("context");

    run_staging_script(&bin_dir, &context);

    for (staged, packaged) in [
        ("ngx.yml", "packaging/config/ngx.yml"),
        ("tun.yml", "packaging/config/tun.yml"),
    ] {
        assert_eq!(
            fs::read(context.join(staged)).expect("the staged configuration"),
            fs::read(repo_root().join(packaged)).expect("the packaged configuration"),
            "{staged} must be the packaged template, byte for byte"
        );
    }
}

/// The staging script refuses a context that holds anything else.
///
/// A stale file from an earlier run would be baked into the image,
/// and the Dockerfile copies by name — so nothing there would notice.
#[test]
fn the_staging_script_refuses_a_context_with_anything_else_in_it() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let bin_dir = dir.path().join("bin");
    fs::create_dir_all(&bin_dir).expect("the binary directory");
    for name in IMAGES {
        fs::write(bin_dir.join(name), b"#!/bin/sh\nexit 0\n").expect("a stand-in binary");
    }
    let context = dir.path().join("context");
    fs::create_dir_all(&context).expect("the build context");
    fs::write(context.join("leftover"), b"stale").expect("a stale file");

    let script = repo_root().join("packaging/container/stage.sh");
    let output = Command::new(host_tool("sh").expect("sh"))
        .arg(&script)
        .arg("--bin-dir")
        .arg(&bin_dir)
        .arg("--out-dir")
        .arg(&context)
        .output()
        .expect("run the staging script");

    assert!(
        !output.status.success(),
        "the staging script must refuse a context that holds a stale file"
    );
}

/// The staging script is POSIX `sh`.
///
/// The release workflow runs it with `sh`, and a bashism would break
/// it on a host whose `/bin/sh` is dash.
#[test]
fn the_staging_script_is_posix_sh() {
    let script = repo_root().join("packaging/container/stage.sh");
    let output = Command::new(host_tool("sh").expect("sh"))
        .arg("-n")
        .arg(&script)
        .output()
        .expect("run sh -n");

    assert!(
        output.status.success(),
        "sh -n must pass on the staging script: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The staging script names the flag it is missing.
///
/// The flag names are the ones the caller typed: `--bin-dir` and
/// `--out-dir`, not the shell variables behind them.  A message that
/// names a flag nobody can type sends the caller looking for a flag
/// that does not exist.
#[test]
fn the_staging_script_names_the_flag_it_is_missing() {
    let script = repo_root().join("packaging/container/stage.sh");
    let dir = tempfile::TempDir::new().expect("temp dir");

    for (arguments, flag) in [
        (vec![], "--bin-dir"),
        (
            vec!["--bin-dir", dir.path().to_str().expect("a path")],
            "--out-dir",
        ),
    ] {
        let output = Command::new(host_tool("sh").expect("sh"))
            .arg(&script)
            .args(&arguments)
            .output()
            .expect("run the staging script");

        assert_eq!(
            output.status.code(),
            Some(2),
            "a missing flag is a usage error: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let message = String::from_utf8_lossy(&output.stderr);
        assert!(
            message.contains(flag),
            "the message must name {flag}: {message}"
        );
    }
}

/// `--help` is an answer, not an error.
///
/// A script that exits non-zero on `--help` cannot be told apart
/// from a failing one by whatever calls it.
#[test]
fn the_staging_script_answers_help_with_success() {
    let script = repo_root().join("packaging/container/stage.sh");
    let output = Command::new(host_tool("sh").expect("sh"))
        .arg(&script)
        .arg("--help")
        .output()
        .expect("run the staging script");

    assert_eq!(
        output.status.code(),
        Some(0),
        "--help must succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("--bin-dir"),
        "--help must print the usage on stdout"
    );
}

/// Run the staging script and insist that it succeeds.
fn run_staging_script(bin_dir: &Path, context: &Path) {
    let script = repo_root().join("packaging/container/stage.sh");
    let output = Command::new(host_tool("sh").expect("sh"))
        .arg(&script)
        .arg("--bin-dir")
        .arg(bin_dir)
        .arg("--out-dir")
        .arg(context)
        .output()
        .expect("run the staging script");

    assert!(
        output.status.success(),
        "the staging script failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
