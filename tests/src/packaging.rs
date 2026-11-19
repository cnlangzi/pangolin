//! Regression tests for the packaging pipeline (issue #176).
//!
//! The packaging tree under `packaging/` is what turns a release into
//! a `.deb` / `.rpm` and a signed apt / rpm repository.  None of it
//! runs in the day-to-day test cycle — nfpm, dpkg, rpm and createrepo
//! are not necessarily installed, and the repository acceptance needs
//! containers.
//!
//! What *is* in the cycle are the parts that are cheap to check and
//! expensive to get wrong:
//!
//!   * the packaged configuration templates have to parse with the
//!     *real* loaders — a template the binary rejects is a package
//!     that installs a configuration the service cannot read;
//!   * the two packaged systemd units have to stay the same unit with
//!     two different paths, and stay in sync with the Ansible units;
//!   * the file list of each package has to be exactly the documented
//!     one — a file that sneaks into a package ends up owned by the
//!     package manager and is overwritten on every upgrade;
//!   * the release workflow has to keep its `package` and
//!     `deploy-repo` jobs;
//!   * the repository metadata script has to keep generating and
//!     signing what the clients verify.
//!
//! The tests need neither the `integration` feature, nor a network,
//! nor root, nor Docker: the renderer is plain `sh` and the
//! assertions are made on the files it writes.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use figment::providers::Format;
use serde_yaml::Value;

use crate::common::{read_repo_file, repo_root};

/// The two packages the pipeline builds.
const PACKAGES: [&str; 2] = ["pangolin-ngx", "pangolin-tun"];

/// The architectures each package is built for.
const ARCHITECTURES: [&str; 2] = ["amd64", "arm64"];

/// The version the tests render with.  A release uses the tag's
/// version; the tests only need *a* version, and one that is not the
/// repository's placeholder.
const TEST_VERSION: &str = "0.3.0";
const TEST_REVISION: &str = "1";
const TEST_GLIBC: &str = "2.34";

// ── helpers ─────────────────────────────────────────────────────────────────

/// A tool of the host, found the way the shell would find it.
fn host_tool(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Render the nfpm configuration of one package for one architecture.
///
/// The renderer is a `sh` script with no dependencies beyond a POSIX
/// shell, so the test drives the real one instead of re-implementing
/// the substitution — a test that re-implements the thing it tests
/// proves nothing.
fn render_nfpm(package: &str, arch: &str, out: &Path) -> String {
    let script = repo_root().join("packaging/render-nfpm.sh");
    let output = Command::new(host_tool("sh").expect("sh"))
        .arg(&script)
        .arg("--package")
        .arg(package)
        .arg("--version")
        .arg(TEST_VERSION)
        .arg("--revision")
        .arg(TEST_REVISION)
        .arg("--arch")
        .arg(arch)
        .arg("--glibc")
        .arg(TEST_GLIBC)
        .arg("--binary")
        .arg(format!("/nonexistent/{package}"))
        .arg("--out")
        .arg(out)
        .output()
        .expect("run the nfpm renderer");
    assert!(
        output.status.success(),
        "the renderer failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    fs::read_to_string(out).expect("read the rendered configuration")
}

/// The permission bits of a rendered `file_info.mode`, as the octal
/// digits the manifest shows.
///
/// YAML 1.1 reads `0644` as the octal number 420, YAML 1.2 as the
/// string "0644", and `0o644` as 420 — so the value is normalised
/// back to digits here and the assertions are made on the digits.
fn mode_of(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => format!("{:04o}", n.as_u64().expect("a numeric mode")),
        other => panic!("the file mode is not a scalar: {other:?}"),
    }
}

/// One entry of a package's file list.
#[derive(Debug, PartialEq, Eq)]
struct Content {
    dst: String,
    kind: String,
    mode: String,
    owner: String,
    group: String,
}

/// The file list of a rendered configuration, read back the way a
/// package manager would see it.
fn contents_of(rendered: &str) -> Vec<Content> {
    let document: Value =
        serde_yaml::from_str(rendered).expect("the rendered configuration is YAML");
    let list = document
        .get("contents")
        .and_then(Value::as_sequence)
        .expect("the rendered configuration has a contents list");

    list.iter()
        .map(|entry| {
            let dst = entry
                .get("dst")
                .and_then(Value::as_str)
                .expect("every entry has a destination")
                .to_string();
            let kind = entry
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("file")
                .to_string();
            let info = entry.get("file_info").expect("every entry has file_info");
            Content {
                dst,
                kind,
                mode: mode_of(info.get("mode").expect("every file_info has a mode")),
                owner: info
                    .get("owner")
                    .and_then(Value::as_str)
                    .expect("every file_info has an owner")
                    .to_string(),
                group: info
                    .get("group")
                    .and_then(Value::as_str)
                    .expect("every file_info has a group")
                    .to_string(),
            }
        })
        .collect()
}

/// The systemd unit file, parsed into `(section, key, value)` triples.
///
/// A unit file is a sequence of sections, each holding `key=value`
/// lines; comments and blank lines carry no meaning.
fn parse_unit(text: &str) -> BTreeMap<(String, String), String> {
    let mut entries = BTreeMap::new();
    let mut section = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.trim().to_string();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        entries.insert(
            (section.clone(), key.trim().to_string()),
            value.trim().to_string(),
        );
    }
    entries
}

/// The `[Service]` section of a unit file.
fn service_section(unit: &BTreeMap<(String, String), String>) -> BTreeMap<String, String> {
    unit.iter()
        .filter(|((section, _), _)| section == "Service")
        .map(|((_, key), value)| (key.clone(), value.clone()))
        .collect()
}

/// The keys of a `[Service]` section.
fn service_keys(unit: &BTreeMap<(String, String), String>) -> Vec<String> {
    service_section(unit).into_keys().collect()
}

// ── the configuration templates ─────────────────────────────────────────────

/// The packaged gateway configuration has to parse with the *real*
/// parser — the same `Config` type the binary deserializes at
/// startup.
///
/// A template the parser rejects is a package that installs a
/// configuration the service cannot read, and the failure surfaces at
/// the first start, on the operator's machine.
///
/// The environment layer is deliberately left out: `Config::from_str`
/// layers the `NGX_*` variables over the YAML, and the environment is
/// process-wide — clearing it here would race with the other tests in
/// this crate.  The literal `Config::from_str` check lives in
/// `crates/pangolin-core/src/config.rs`, where the environment can be
/// sandboxed safely.
#[test]
fn the_gateway_config_template_parses_with_the_real_parser() {
    let template = read_repo_file("packaging/config/ngx.yml");

    let config: pangolin_core::config::Config = figment::Figment::new()
        .merge(figment::providers::Yaml::string(&template))
        .extract()
        .expect("the packaged ngx.yml must parse with the real parser");

    assert_eq!(config.addr.http, "0.0.0.0:80");
    assert_eq!(config.addr.https, "0.0.0.0:443");
    assert_eq!(config.acme.cert_dir, "/var/lib/pangolin/certs");
    assert_eq!(
        config.acme.acme_directory,
        "https://acme-v02.api.letsencrypt.org/directory"
    );
    assert_eq!(config.acme.renew_threshold_days, 14);
    assert_eq!(config.acme.renew_check_interval_hours, 6);
    assert_eq!(config.acme.key_type, "ecdsa");
    assert_eq!(
        config.log.bot.dir,
        PathBuf::from("/var/log/pangolin/bots"),
        "the bot log directory has to be absolute — a relative path follows the \
         service's working directory"
    );
    assert_eq!(
        config.log.bot.cache_dir,
        PathBuf::from("/var/cache/pangolin/bots")
    );
    assert_eq!(
        config.log.referer.dir,
        PathBuf::from("/var/log/pangolin/referers")
    );
}

/// The packaged tunnel configuration has to parse with the real parser
/// too.
///
/// The shipped template carries an empty `token` — the operator has to
/// fill it in — so the template is parsed twice: once as it ships, to
/// pin that the empty token is the *only* thing that keeps it from
/// loading, and once with a token, to prove the rest of the file is
/// well formed.
#[test]
fn the_tunnel_config_template_parses_with_the_real_parser() {
    let template = read_repo_file("packaging/config/tun.yml");

    // The YAML layer is merged directly, not through
    // `TunConfig::from_str`: that parser layers the `TUN_*` environment
    // over the document, and the environment is process-wide — a
    // `TUN_TOKEN` exported by the developer's shell (the repository's
    // own `.env.example` ships one) would decide what the test sees.
    // The validation is the parser's own, called directly.
    let empty: tun::config::TunConfig = figment::Figment::new()
        .merge(figment::providers::Yaml::string(&template))
        .extract()
        .expect("the packaged tun.yml must be a valid document");
    let error = tun::config::validate(&empty).expect_err("an empty token must be refused");
    assert!(
        error.to_string().contains("token"),
        "the empty token must be the reason the template does not load: {error}"
    );

    let filled = template.replace("token: \"\"", "token: \"a-real-token\"");
    let config: tun::config::TunConfig = figment::Figment::new()
        .merge(figment::providers::Yaml::string(&filled))
        .extract()
        .expect("the packaged tun.yml must be a valid document");
    tun::config::validate(&config)
        .expect("the packaged tun.yml must be valid once the token is filled in");

    assert_eq!(config.server, "127.0.0.1:9001");
    assert_eq!(config.name, "local");
    assert_eq!(
        config.transport,
        pangolin_tun::TransportSelector::Auto,
        "the packaged tunnel node prefers QUIC and falls back to the TCP transport"
    );
    assert_eq!(
        config.pin_path,
        Some(PathBuf::from("/var/lib/pangolin-tun/tun.pins")),
        "the pin file has to live under /var/lib — the derived path would put it in \
         /etc/pangolin, which the service user cannot write to"
    );
    assert_eq!(
        config.quic_failure_path,
        Some(PathBuf::from("/var/lib/pangolin-tun/tun.quic-fail"))
    );
}

/// Neither template may carry a relative path.
///
/// A relative path in the packaged configuration follows the service's
/// working directory — `/var/lib/pangolin` — instead of meaning what
/// it says, and the operator has no way of seeing that from the file.
#[test]
fn the_config_templates_carry_no_relative_paths() {
    for template in ["packaging/config/ngx.yml", "packaging/config/tun.yml"] {
        let text = read_repo_file(template);
        for (number, line) in text.lines().enumerate() {
            let code = line.split('#').next().unwrap_or("");
            assert!(
                !code.contains("./"),
                "{template}:{} carries a relative path: {line}",
                number + 1
            );
        }
    }
}

/// Every path the templates name is absolute.
///
/// The `./` check above is a proxy: a path like `logs/bots` is
/// relative too, and it would slip through.  The keys that hold a
/// path are listed here, and each of them has to start with a `/`.
#[test]
fn every_path_in_the_config_templates_is_absolute() {
    let ngx: pangolin_core::config::Config = figment::Figment::new()
        .merge(figment::providers::Yaml::string(&read_repo_file(
            "packaging/config/ngx.yml",
        )))
        .extract()
        .expect("the packaged ngx.yml must be a valid document");
    let tun: tun::config::TunConfig = figment::Figment::new()
        .merge(figment::providers::Yaml::string(&read_repo_file(
            "packaging/config/tun.yml",
        )))
        .extract()
        .expect("the packaged tun.yml must be a valid document");

    let mut paths: Vec<PathBuf> = vec![
        PathBuf::from(&ngx.acme.cert_dir),
        ngx.log.bot.dir.clone(),
        ngx.log.bot.cache_dir.clone(),
        ngx.log.referer.dir.clone(),
    ];
    if let Some(pin) = &tun.pin_path {
        paths.push(pin.clone());
    }
    if let Some(quic) = &tun.quic_failure_path {
        paths.push(quic.clone());
    }

    for path in paths {
        assert!(
            path.is_absolute(),
            "{path:?} is relative — it would follow the service's working directory"
        );
    }
}

/// Neither template may carry a retired key.
///
/// The configuration loader does *not* refuse unknown keys: a stale
/// key is silently ignored with a warning.  The template is the only
/// place an operator learns a knob exists, so it has to be clean on
/// its own — `log.bot.enabled` was retired in issue #117.
#[test]
fn the_config_templates_carry_no_retired_keys() {
    for template in ["packaging/config/ngx.yml", "packaging/config/tun.yml"] {
        let text = read_repo_file(template);
        assert!(
            !text.contains("log.bot.enabled"),
            "{template} carries the retired `log.bot.enabled` key — the loader ignores it \
             silently, so the template must not mention it"
        );
    }
}

// ── the systemd units ───────────────────────────────────────────────────────

/// The two packaged units are the same unit with two different paths.
///
/// They carry the same `[Service]` keys, and every value but the two
/// path keys is identical — the capability set, the restart policy and
/// the limits have to mean the same thing on the gateway and on the
/// tunnel node.
#[test]
fn the_packaged_units_share_their_service_keys() {
    let ngx = parse_unit(&read_repo_file("packaging/systemd/pangolin-ngx.service"));
    let tun = parse_unit(&read_repo_file("packaging/systemd/pangolin-tun.service"));

    let ngx_service = service_section(&ngx);
    let tun_service = service_section(&tun);

    assert_eq!(
        service_keys(&ngx),
        service_keys(&tun),
        "the two packaged units must carry the same [Service] keys"
    );

    for (key, value) in &ngx_service {
        if key == "WorkingDirectory" || key == "ExecStart" {
            continue;
        }
        assert_eq!(
            tun_service.get(key),
            Some(value),
            "`{key}` differs between the two packaged units"
        );
    }

    assert_eq!(
        ngx_service.get("WorkingDirectory").map(String::as_str),
        Some("/var/lib/pangolin")
    );
    assert_eq!(
        tun_service.get("WorkingDirectory").map(String::as_str),
        Some("/var/lib/pangolin-tun")
    );
    assert_eq!(
        ngx_service.get("ExecStart").map(String::as_str),
        Some("/usr/bin/pangolin-ngx --config /etc/pangolin/ngx.yml")
    );
    assert_eq!(
        tun_service.get("ExecStart").map(String::as_str),
        Some("/usr/bin/pangolin-tun --config /etc/pangolin/tun.yml")
    );
}

/// The gateway runs unprivileged and binds the privileged ports
/// through a capability.
///
/// Without `AmbientCapabilities=CAP_NET_BIND_SERVICE` the binary
/// panics with "Permission denied" on `bind()` — ports below 1024 are
/// reserved for root — and the operator is left with a service that
/// dies on every start.
#[test]
fn the_gateway_unit_runs_unprivileged_with_the_bind_capability() {
    let ngx = service_section(&parse_unit(&read_repo_file(
        "packaging/systemd/pangolin-ngx.service",
    )));

    assert_eq!(ngx.get("User").map(String::as_str), Some("pangolin"));
    assert_eq!(ngx.get("Group").map(String::as_str), Some("pangolin"));
    assert_eq!(
        ngx.get("AmbientCapabilities").map(String::as_str),
        Some("CAP_NET_BIND_SERVICE")
    );
    assert_eq!(
        ngx.get("CapabilityBoundingSet").map(String::as_str),
        Some("CAP_NET_BIND_SERVICE")
    );
}

/// The packaged units have to stay in sync with the Ansible ones.
///
/// The Ansible role is the other deployment path, and it is not
/// going away: the two have to mean the same thing, so a change to
/// one that is not made to the other is a bug.
///
/// They differ in exactly two ways on purpose: their paths, and the
/// capability set — the tunnel node's unit grants
/// `CAP_NET_BIND_SERVICE` too, so that both packaged units carry the
/// same `[Service]` key set.  Anything else that drifts apart is a
/// bug, and the check below says so.
#[test]
fn the_packaged_units_stay_in_sync_with_the_ansible_units() {
    for (package, ansible) in [
        (
            "pangolin-ngx",
            "deploy/playbooks/roles/ngx/files/ngx.service",
        ),
        (
            "pangolin-tun",
            "deploy/playbooks/roles/tun/files/tun.service",
        ),
    ] {
        let packaged = service_section(&parse_unit(&read_repo_file(&format!(
            "packaging/systemd/{package}.service"
        ))));
        let ansible_unit = service_section(&parse_unit(&read_repo_file(ansible)));

        for (key, value) in &ansible_unit {
            assert!(
                packaged.contains_key(key),
                "{package}.service is missing `{key}` — the Ansible unit has it"
            );
            if key == "WorkingDirectory" || key == "ExecStart" {
                // The paths are the one thing that differs on purpose.
                continue;
            }
            assert_eq!(
                packaged.get(key),
                Some(value),
                "`{key}` differs between {package}.service and {ansible}"
            );
        }

        // And the other way round: the packaged unit may add the
        // capability lines, but nothing else.
        for key in packaged.keys() {
            if ansible_unit.contains_key(key) {
                continue;
            }
            assert!(
                matches!(
                    key.as_str(),
                    "AmbientCapabilities" | "CapabilityBoundingSet"
                ),
                "{package}.service adds `{key}` to {ansible} — the two units are the same \
                 unit with two different paths and one capability set"
            );
        }
    }
}

// ── the file lists ──────────────────────────────────────────────────────────

/// The file list of each package is exactly the documented one.
///
/// A file that is not in the list does not ship; a file that is in the
/// list but should not be ends up owned by the package manager, which
/// overwrites it on every upgrade.  The runtime state — the SQLite
/// database, the certificates, the logs — is written by the running
/// service and is deliberately *not* in the package.
#[test]
fn the_package_file_lists_match_the_documented_layout() {
    let expected_ngx = vec![
        Content {
            dst: "/usr/bin/pangolin-ngx".into(),
            kind: "file".into(),
            mode: "0755".into(),
            owner: "root".into(),
            group: "root".into(),
        },
        Content {
            dst: "/usr/lib/systemd/system/pangolin-ngx.service".into(),
            kind: "file".into(),
            mode: "0644".into(),
            owner: "root".into(),
            group: "root".into(),
        },
        Content {
            dst: "/etc/pangolin/ngx.yml".into(),
            kind: "config|noreplace".into(),
            mode: "0640".into(),
            owner: "root".into(),
            group: "pangolin".into(),
        },
        Content {
            dst: "/etc/tmpfiles.d/pangolin.conf".into(),
            kind: "file".into(),
            mode: "0644".into(),
            owner: "root".into(),
            group: "root".into(),
        },
        Content {
            dst: "/var/lib/pangolin".into(),
            kind: "dir".into(),
            mode: "0750".into(),
            owner: "pangolin".into(),
            group: "pangolin".into(),
        },
        Content {
            dst: "/var/log/pangolin/bots".into(),
            kind: "dir".into(),
            mode: "0750".into(),
            owner: "pangolin".into(),
            group: "pangolin".into(),
        },
        Content {
            dst: "/var/log/pangolin/referers".into(),
            kind: "dir".into(),
            mode: "0750".into(),
            owner: "pangolin".into(),
            group: "pangolin".into(),
        },
        Content {
            dst: "/var/cache/pangolin/bots".into(),
            kind: "dir".into(),
            mode: "0750".into(),
            owner: "pangolin".into(),
            group: "pangolin".into(),
        },
    ];

    let expected_tun = vec![
        Content {
            dst: "/usr/bin/pangolin-tun".into(),
            kind: "file".into(),
            mode: "0755".into(),
            owner: "root".into(),
            group: "root".into(),
        },
        Content {
            dst: "/usr/lib/systemd/system/pangolin-tun.service".into(),
            kind: "file".into(),
            mode: "0644".into(),
            owner: "root".into(),
            group: "root".into(),
        },
        Content {
            dst: "/etc/pangolin/tun.yml".into(),
            kind: "config|noreplace".into(),
            mode: "0640".into(),
            owner: "root".into(),
            group: "pangolin".into(),
        },
        Content {
            dst: "/var/lib/pangolin-tun".into(),
            kind: "dir".into(),
            mode: "0750".into(),
            owner: "pangolin".into(),
            group: "pangolin".into(),
        },
    ];

    let dir = tempfile::TempDir::new().expect("temp dir");
    for (package, expected) in [
        ("pangolin-ngx", &expected_ngx),
        ("pangolin-tun", &expected_tun),
    ] {
        let rendered = render_nfpm(
            package,
            "amd64",
            &dir.path().join(format!("nfpm-{package}.yaml")),
        );
        assert_eq!(
            contents_of(&rendered),
            *expected,
            "the file list of {package} does not match the documented layout"
        );
    }
}

/// The two packages own disjoint paths.
///
/// dpkg refuses to install two packages that own the same file, and
/// the gateway and the tunnel node are installed on different machines
/// anyway — a source host that already runs its own web server must
/// be able to install the tunnel node without the gateway's files
/// getting in the way.
#[test]
fn the_two_packages_own_disjoint_paths() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let mut paths: BTreeMap<String, String> = BTreeMap::new();

    for package in PACKAGES {
        let rendered = render_nfpm(
            package,
            "amd64",
            &dir.path().join(format!("nfpm-{package}.yaml")),
        );
        for content in contents_of(&rendered) {
            if let Some(owner) = paths.insert(content.dst.clone(), package.to_string()) {
                panic!(
                    "{} is owned by both {owner} and {package} — the two packages \
                     must not share a path",
                    content.dst
                );
            }
        }
    }
}

/// The rendered configuration declares the measured glibc floor.
///
/// The floor is read out of the binary that is about to be packaged,
/// never written down by hand: a stale floor either keeps the package
/// off a host that could run it, or lets it onto a host that cannot.
#[test]
fn the_rendered_configuration_declares_the_measured_glibc_floor() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    for package in PACKAGES {
        for arch in ARCHITECTURES {
            let rendered = render_nfpm(
                package,
                arch,
                &dir.path().join(format!("nfpm-{package}-{arch}.yaml")),
            );
            let document: Value =
                serde_yaml::from_str(&rendered).expect("the rendered configuration is YAML");

            let deb = document
                .get("overrides")
                .and_then(|o| o.get("deb"))
                .and_then(|d| d.get("depends"))
                .and_then(Value::as_sequence)
                .expect("the deb override has a dependency list");
            let rpm = document
                .get("overrides")
                .and_then(|o| o.get("rpm"))
                .and_then(|r| r.get("depends"))
                .and_then(Value::as_sequence)
                .expect("the rpm override has a dependency list");

            let deb_depends: Vec<&str> = deb.iter().filter_map(Value::as_str).collect();
            let rpm_depends: Vec<&str> = rpm.iter().filter_map(Value::as_str).collect();

            assert!(
                deb_depends.contains(&format!("libc6 (>= {TEST_GLIBC})").as_str()),
                "the deb dependency list of {package}/{arch} is {deb_depends:?}"
            );
            assert!(
                deb_depends.contains(&"ca-certificates"),
                "the deb dependency list of {package}/{arch} is {deb_depends:?}"
            );
            assert!(
                rpm_depends.contains(&format!("glibc >= {TEST_GLIBC}").as_str()),
                "the rpm dependency list of {package}/{arch} is {rpm_depends:?}"
            );
            assert!(
                rpm_depends.contains(&"ca-certificates"),
                "the rpm dependency list of {package}/{arch} is {rpm_depends:?}"
            );
        }
    }
}

// ── the release workflow ────────────────────────────────────────────────────

/// The release workflow keeps its `package` and `deploy-repo` jobs.
///
/// `package` turns the binaries into packages; `deploy-repo` turns the
/// packages into a signed repository.  A tag that publishes a release
/// without them ships archives only — the repository would silently
/// keep serving the previous version.
#[test]
fn the_release_workflow_has_the_package_and_deploy_repo_jobs() {
    let workflow = read_repo_file(".github/workflows/release.yml");

    for job in ["package:", "deploy-repo:"] {
        assert!(
            workflow.contains(&format!("\n  {job}")),
            "the release workflow must have a `{job}` job"
        );
    }

    // The publishing job waits for the packages, and the repository
    // job waits for the publishing job — a repository must never be
    // published before the release it mirrors.
    assert!(
        workflow.contains("needs: [build, build-mac, build-tun, package]"),
        "the release job must depend on the package job"
    );
    assert!(
        workflow.contains("needs: [release]"),
        "the deploy-repo job must depend on the release job"
    );

    // The packages travel to the release as artifacts.
    for pattern in ["dist/*.deb", "dist/*.rpm"] {
        assert!(
            workflow.contains(pattern),
            "the release must attach {pattern}"
        );
    }
}

/// The repository job is the only one that may push, and it pushes
/// one branch at a time.
///
/// Two releases pushing the `gh-pages` branch at the same time would
/// race, and the loser would leave the site tree half-written.  The
/// concurrency group serializes the pushes; `cancel-in-progress:
/// false` keeps the first one from being cancelled halfway.
#[test]
fn the_deploy_repo_job_pushes_the_gh_pages_branch_under_a_concurrency_group() {
    let workflow = read_repo_file(".github/workflows/release.yml");

    let deploy_repo = workflow
        .split("\n  deploy-repo:")
        .nth(1)
        .expect("the workflow has a deploy-repo job");

    assert!(
        deploy_repo.contains("contents: write"),
        "the deploy-repo job needs write access to push the gh-pages branch"
    );
    assert!(
        deploy_repo.contains("group: pages-deploy"),
        "the deploy-repo job must serialize its pushes with a concurrency group"
    );
    assert!(
        deploy_repo.contains("cancel-in-progress: false"),
        "the deploy-repo job must not cancel a push that is already running"
    );
    assert!(
        deploy_repo.contains("HEAD:gh-pages"),
        "the deploy-repo job must push the gh-pages branch"
    );
    assert!(
        deploy_repo.contains("git fetch origin gh-pages"),
        "the deploy-repo job must fetch the gh-pages branch before replacing it"
    );
}

/// The marker of an armored private key block.
///
/// Assembled at runtime: the literal must not appear anywhere in the
/// repository — not even in this file — or a `git grep` for it would
/// find this very test and the check would be meaningless.
fn private_key_marker() -> String {
    ["-----BEGIN PGP ", "PRIVATE", " KEY BLOCK-----"].concat()
}

/// The private key never reaches the repository.
///
/// The archive key's *public* half is committed — the operator needs
/// it to verify the repository — but the private half lives in a
/// repository secret.  A private key block in the tree would be
/// published by the very release that is supposed to sign it.
#[test]
fn the_archive_private_key_is_not_in_the_repository() {
    let marker = private_key_marker();

    let key = read_repo_file("packaging/keys/pangolin-archive-keyring.asc");
    assert!(
        key.starts_with("-----BEGIN PGP PUBLIC KEY BLOCK-----"),
        "the committed key must be the public one"
    );
    assert!(
        !key.contains(&marker),
        "the committed key must not be a private key"
    );

    // The private key is not in the tree either.
    let mut offenders = Vec::new();
    for (path, text) in tracked_text_files() {
        if text.contains(&marker) {
            offenders.push(path);
        }
    }
    assert!(
        offenders.is_empty(),
        "a private key block is in the repository: {offenders:?}"
    );
}

/// Every text file of the repository, read for the private-key check.
fn tracked_text_files() -> Vec<(String, String)> {
    let root = repo_root();
    let mut files = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || name == "target" || name == "dist" || name == "bin" {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if let Ok(text) = fs::read_to_string(&path) {
                files.push((
                    path.strip_prefix(&root)
                        .unwrap_or(&path)
                        .display()
                        .to_string(),
                    text,
                ));
            }
        }
    }
    files
}

// ── the repository metadata ─────────────────────────────────────────────────

/// The repository script generates the metadata the clients verify.
///
/// The apt side is `Packages` + a hand-written `Release`, signed twice
/// (`InRelease` for modern apt, `Release.gpg` for older clients); the
/// rpm side is `createrepo_c` over the signed packages.  A script that
/// stops doing any of that leaves the clients with a repository they
/// cannot verify.
#[test]
fn the_publish_script_generates_and_signs_the_metadata() {
    let script = read_repo_file("packaging/publish-repo.sh");

    for needle in [
        "dpkg-scanpackages --arch",
        "--multiversion",
        "createrepo_c",
        "--clearsign",
        "--detach-sign",
        "Packages.gz",
    ] {
        assert!(
            script.contains(needle),
            "publish-repo.sh must contain `{needle}`"
        );
    }

    // The two signatures carry the same text: `InRelease` is the
    // clearsigned `Release`, `Release.gpg` its detached signature.
    assert!(
        script.contains("--clearsign --output \"$dists/InRelease\" \"$release\""),
        "InRelease must be the clearsigned Release"
    );
    assert!(
        script.contains("--detach-sign --output \"$dists/Release.gpg\" \"$release\""),
        "Release.gpg must be the detached signature of the same Release"
    );

    // The rpm packages are signed *before* the metadata is generated:
    // the metadata records the hash of the package, and a hash taken
    // before the signature was added does not match the file that ends
    // up in the repository.
    let signs = script.find("--addsign").expect("the packages are signed");
    let creates = script
        .find("createrepo_c \"$rpm_root\"")
        .expect("the metadata is generated");
    assert!(
        signs < creates,
        "the packages must be signed before createrepo_c records their hashes"
    );
}

/// The published key has to be the key that signed the repository.
///
/// The operator trusts the key the site publishes; the signatures are
/// made with the key from the secret.  If the two ever diverge, every
/// signature in the published repository is unverifiable — `NO_PUBKEY`
/// on `apt update` — while the publishing job still reports success.
/// The script compares the fingerprints and refuses to publish.
#[test]
fn the_publish_script_refuses_a_key_that_did_not_sign() {
    let Some(gpg) = host_tool("gpg") else {
        // Without gpg there is nothing to sign with and nothing to
        // compare; the check is exercised in the CI job.
        return;
    };

    let dir = tempfile::TempDir::new().expect("temp dir");
    let home = dir.path().join("gnupg");
    fs::create_dir_all(&home).expect("create the gpg home");

    // A key that is *not* the committed one.
    let generated = Command::new(&gpg)
        .arg("--homedir")
        .arg(&home)
        .arg("--batch")
        .arg("--passphrase")
        .arg("")
        .arg("--quick-generate-key")
        .arg("Someone Else <other@example.invalid>")
        .arg("rsa2048")
        .arg("sign")
        .arg("never")
        .output()
        .expect("generate a throwaway key");
    assert!(
        generated.status.success(),
        "could not generate the throwaway key:\n{}",
        String::from_utf8_lossy(&generated.stderr)
    );

    let output = Command::new(host_tool("sh").expect("sh"))
        .arg(repo_root().join("packaging/publish-repo.sh"))
        .arg("--site")
        .arg(dir.path().join("site"))
        .arg("--deb-dir")
        .arg(dir.path().join("debs"))
        .arg("--rpm-dir")
        .arg(dir.path().join("rpms"))
        .arg("--version")
        .arg("0.3.0")
        // The committed key — the throwaway one is what signs.
        .arg("--key")
        .arg(repo_root().join("packaging/keys/pangolin-archive-keyring.asc"))
        .arg("--sign")
        .env("GNUPGHOME", &home)
        .output()
        .expect("run the publish script");

    assert!(
        !output.status.success(),
        "the script must refuse to publish a repository signed with a different key"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("is not the key that signed the repository"),
        "the failure must name the mismatch:\n{stderr}"
    );
}

/// The build script measures the glibc floor instead of hardcoding it.
#[test]
fn the_build_script_measures_the_glibc_floor() {
    let script = read_repo_file("packaging/build-packages.sh");

    assert!(
        script.contains("objdump -T"),
        "the build script must read the symbol versions out of the binary"
    );
    assert!(
        script.contains("GLIBC_"),
        "the build script must look for the GLIBC symbol versions"
    );
    assert!(
        script.contains("--glibc \"$glibc\""),
        "the measured floor must reach the rendered configuration"
    );
}

/// The package version is derived from the release tag, and a
/// pre-release sorts *before* the release it precedes.
///
/// Debian's version order puts `1.2.3~rc1-1` before `1.2.3-1`; a
/// pre-release published as `1.2.3-rc1-1` would sort *after* it and
/// `apt upgrade` would never move a machine onto it.
///
/// The derivation is the *script's*, not a re-implementation of it:
/// the test runs `build-packages.sh` with a fake nfpm and reads the
/// version out of the configuration it rendered.
#[test]
fn a_prerelease_sorts_before_the_release_it_precedes() {
    for (tag, expected) in [
        ("0.3.0", "0.3.0"),
        ("1.2.3-rc1", "1.2.3~rc1"),
        // A `-` may not appear in the upstream part of a Debian
        // version, so a pre-release that carries one has it folded
        // into a `+`.
        ("1.2.3-rc-1", "1.2.3~rc+1"),
    ] {
        let version = rendered_version_for(tag);
        assert_eq!(version, expected, "the tag {tag} must map to {expected}");

        // And the ordering the mapping exists for.
        if let Some(dpkg) = host_tool("dpkg") {
            let output = Command::new(dpkg)
                .arg("--compare-versions")
                .arg(format!("{expected}-1"))
                .arg("lt")
                .arg("1.2.3-1")
                .output()
                .expect("run dpkg --compare-versions");
            assert!(
                output.status.success(),
                "{expected}-1 must sort before 1.2.3-1 in Debian's version order"
            );
        }
    }
}

/// The version `build-packages.sh` derives from a tag, read out of
/// the configuration it rendered.
///
/// The fake nfpm records the configuration it was handed and leaves
/// two files behind — the build script checks that a deb and an rpm
/// came out, and the fake `objdump` reports a fixed symbol version so
/// the glibc measurement succeeds without a real binary.
fn rendered_version_for(tag: &str) -> String {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let bin = dir.path().join("bin");
    fs::create_dir_all(&bin).expect("create the fake bin directory");

    let recorded = dir.path().join("rendered.yaml");
    write_fake(
        &bin,
        "nfpm",
        &format!(
            "#!/bin/sh\n\
config=\"\"\n\
while [ $# -gt 0 ]; do\n\
    case \"$1\" in\n\
        --config) config=\"$2\"; shift ;;\n\
    esac\n\
    shift\n\
done\n\
cp \"$config\" \"{recorded}\"\n\
touch pangolin-test.deb pangolin-test.rpm\n",
            recorded = recorded.display()
        ),
    );
    write_fake(
        &bin,
        "objdump",
        "#!/bin/sh\necho '0000000000000000 v GLIBC_2.34 (GLIBC_2.34)'\n",
    );

    // The script only needs the binary to exist; the fake objdump
    // never reads it.
    let binaries = dir.path().join("bin-input");
    fs::create_dir_all(&binaries).expect("create the binary directory");
    for name in ["pangolin-ngx", "pangolin-tun"] {
        fs::write(binaries.join(name), "#!/bin/sh\n").expect("write the fake binary");
    }

    let output = Command::new(host_tool("sh").expect("sh"))
        .arg(repo_root().join("packaging/build-packages.sh"))
        .arg("--tag-version")
        .arg(tag)
        .arg("--arch")
        .arg("amd64")
        .arg("--bin-dir")
        .arg(&binaries)
        .arg("--out-dir")
        .arg(dir.path().join("out"))
        // The fakes come first, the host's tools stay reachable: the
        // build script needs `dirname`, `mkdir` and friends.
        .env(
            "PATH",
            std::env::join_paths(std::iter::once(bin.clone()).chain(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            )))
            .expect("build the PATH"),
        )
        .env("NFPM", bin.join("nfpm"))
        .output()
        .expect("run the build script");
    assert!(
        output.status.success(),
        "build-packages.sh must succeed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let rendered = fs::read_to_string(&recorded).expect("read the rendered configuration");
    let document: serde_yaml::Value =
        serde_yaml::from_str(&rendered).expect("the rendered configuration is YAML");
    document
        .get("version")
        .and_then(Value::as_str)
        .expect("the rendered configuration has a version")
        .to_string()
}

// ── the lifecycle scripts ───────────────────────────────────────────────────

/// A throwaway root the lifecycle scripts are driven against.
///
/// The scripts touch absolute paths — `/usr/lib/systemd/system`,
/// `/etc/pangolin`, `/var/lib/pangolin` — so they read them through
/// `PANGOLIN_ROOT`, which is empty in production and points here in
/// the test.  The fake tools in `bin` record what the scripts tried
/// to do, so the test can assert on the calls without a systemd and
/// without root.
struct Sandbox {
    dir: tempfile::TempDir,
    bin: PathBuf,
    calls: PathBuf,
    group_exists: bool,
    user_exists: bool,
}

impl Sandbox {
    /// A sandbox with the given unit files installed, and with neither
    /// the `pangolin` group nor the `pangolin` user in existence.
    fn with_units(units: &[&str]) -> Self {
        Self::with_state(units, false, false)
    }

    /// A sandbox with the given unit files installed and the given
    /// state of the `pangolin` group and user.
    ///
    /// The state matters: the Ansible role and `install.sh` create
    /// that user, so a host that already has one is the normal case,
    /// not the exotic one.
    fn with_state(units: &[&str], group_exists: bool, user_exists: bool) -> Self {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).expect("create the fake bin directory");

        let calls = dir.path().join("calls");
        fs::write(&calls, "").expect("create the call log");

        // The real tools the scripts need, and the fakes for the
        // ones that would touch the machine.
        for tool in ["mkdir", "ln", "dirname", "cat", "rm"] {
            link_tool(tool, &bin);
        }
        write_fake(&bin, "systemctl", &fake_systemctl(&calls));
        write_fake(&bin, "chown", "#!/bin/sh\nexit 0\n");
        write_fake(&bin, "chmod", "#!/bin/sh\nexit 0\n");
        write_fake(
            &bin,
            "id",
            "#!/bin/sh\n[ \"${FAKE_USER_EXISTS:-0}\" = 1 ] && exit 0\nexit 1\n",
        );
        write_fake(
            &bin,
            "getent",
            "#!/bin/sh\n[ \"${FAKE_GROUP_EXISTS:-0}\" = 1 ] && exit 0\nexit 2\n",
        );
        write_fake(&bin, "useradd", &fake_useradd(&calls));
        write_fake(&bin, "groupadd", &fake_groupadd(&calls));

        for unit in units {
            let path = dir.path().join("usr/lib/systemd/system").join(unit);
            fs::create_dir_all(path.parent().expect("a parent")).expect("create the unit dir");
            fs::write(&path, "[Service]\n").expect("write the unit");
        }

        Sandbox {
            dir,
            bin,
            calls,
            group_exists,
            user_exists,
        }
    }

    /// The root the scripts are pointed at.
    fn root(&self) -> &Path {
        self.dir.path()
    }

    /// Run one of the lifecycle scripts.
    fn run(&self, script: &str, args: &[&str]) -> Output {
        Command::new(host_tool("sh").expect("sh"))
            .arg(repo_root().join("packaging/scripts").join(script))
            .args(args)
            .env("PATH", &self.bin)
            .env("PANGOLIN_ROOT", self.root())
            .env("HOME", self.dir.path())
            .env("TMPDIR", self.dir.path())
            .env(
                "FAKE_GROUP_EXISTS",
                if self.group_exists { "1" } else { "0" },
            )
            .env("FAKE_USER_EXISTS", if self.user_exists { "1" } else { "0" })
            .env("FAKE_USER_NAME", "pangolin")
            .output()
            .expect("run the lifecycle script")
    }

    /// The calls the fake tools recorded.
    fn calls(&self) -> String {
        fs::read_to_string(&self.calls).expect("read the call log")
    }

    /// The "wants" symlink of a unit, if the script created one.
    fn wants_link(&self, unit: &str) -> Option<PathBuf> {
        let link = self
            .root()
            .join("etc/systemd/system/multi-user.target.wants")
            .join(unit);
        link.exists().then_some(link)
    }
}

/// Copy a tool of the host into the sandbox's bin directory.
fn link_tool(name: &str, bin: &Path) {
    let Some(source) = host_tool(name) else {
        return;
    };
    let _ = fs::copy(&source, bin.join(name));
    let _ = fs::set_permissions(
        bin.join(name),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    );
}

/// Write a fake tool into the sandbox's bin directory.
fn write_fake(bin: &Path, name: &str, body: &str) {
    let path = bin.join(name);
    fs::write(&path, body).expect("write the fake");
    let _ = fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755));
}

/// A fake `systemctl` that records its arguments.
fn fake_systemctl(calls: &Path) -> String {
    format!(
        "#!/bin/sh\necho \"systemctl $*\" >> {calls}\nexit 0\n",
        calls = calls.display()
    )
}

/// A fake `useradd` that records its arguments.
///
/// It also reproduces the one behaviour of the real tool the
/// preinstall script has to get right: `--user-group` asks useradd to
/// *create* the group, and useradd refuses when a group of that name
/// already exists — which it always does by the time the script
/// calls it.  A stub that always succeeded would let the broken
/// invocation through.
fn fake_useradd(calls: &Path) -> String {
    let body = r#"#!/bin/sh
echo "useradd $*" >> {calls}
group=""
for arg in "$@"; do
    case "$arg" in
        --user-group|-U) group=created ;;
        --gid|--gid=*) group=given ;;
    esac
done
if [ "$group" = created ] && [ "${FAKE_GROUP_EXISTS:-0}" = 1 ]; then
    echo "useradd: group ${FAKE_USER_NAME:-pangolin} exists - if you want to add this user to that group, use -g." >&2
    exit 9
fi
exit 0
"#;
    body.replace("{calls}", &calls.display().to_string())
}

/// A fake `groupadd` that records its arguments.
fn fake_groupadd(calls: &Path) -> String {
    format!(
        "#!/bin/sh\necho \"groupadd $*\" >> {calls}\nexit 0\n",
        calls = calls.display()
    )
}

/// The gateway is enabled, the tunnel node is not.
///
/// A tunnel node whose `(name, token)` row does not exist in the
/// gateway's admin UI yet is rejected at the handshake: enabling it
/// would leave the unit restart-looping on a host where the operator
/// has not finished the setup.
#[test]
fn the_postinstall_enables_the_gateway_and_not_the_tunnel_node() {
    let sandbox = Sandbox::with_units(&["pangolin-ngx.service"]);
    let output = sandbox.run("postinstall.sh", &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "the postinstall must not fail:\n{stdout}{stderr}"
    );
    assert!(
        sandbox.wants_link("pangolin-ngx.service").is_some(),
        "the gateway must be enabled for the next boot"
    );
    assert!(
        sandbox.wants_link("pangolin-tun.service").is_none(),
        "the tunnel node must not be enabled"
    );

    // The operator is told what is left to do.
    assert!(
        stdout.contains("/etc/pangolin/ngx.yml"),
        "the postinstall must name the configuration file:\n{stdout}"
    );
    assert!(
        stdout.contains("systemctl start pangolin-ngx"),
        "the postinstall must say how to start the gateway:\n{stdout}"
    );

    // The service is enabled, never started.
    let calls = sandbox.calls();
    assert!(
        !calls.contains("systemctl start"),
        "the postinstall must not start anything:\n{calls}"
    );
}

/// An upgrade does not re-enable the service.
///
/// dpkg calls `postinst configure <old-version>` on an upgrade and
/// rpm's `%post` gets `$1=2`.  An operator who deliberately ran
/// `systemctl disable` must not find the unit enabled again after the
/// next package upgrade.
#[test]
fn the_postinstall_leaves_the_enable_state_alone_on_an_upgrade() {
    for arguments in [vec!["configure", "0.3.0-1"], vec!["2"]] {
        let sandbox = Sandbox::with_units(&["pangolin-ngx.service"]);
        let output = sandbox.run("postinstall.sh", &arguments);
        assert!(
            output.status.success(),
            "the postinstall must not fail on an upgrade:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            sandbox.wants_link("pangolin-ngx.service").is_none(),
            "an upgrade must not enable the unit again: {arguments:?}"
        );
    }
}

/// An upgrade restarts a running service — but never starts one.
///
/// The binary has been replaced, and the running process still holds
/// the old executable: `Restart=on-failure` never fires, so the new
/// binary would not take effect until the operator restarted the
/// service by hand.  `try-restart` restarts the unit only when it is
/// running, so the "the package never starts the service" contract
/// holds.
#[test]
fn the_postinstall_restarts_a_running_service_on_an_upgrade() {
    let sandbox = Sandbox::with_units(&["pangolin-ngx.service"]);
    let output = sandbox.run("postinstall.sh", &["configure", "0.3.0-1"]);
    assert!(
        output.status.success(),
        "the postinstall must not fail on an upgrade:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let calls = sandbox.calls();
    assert!(
        calls.contains("systemctl try-restart pangolin-ngx.service"),
        "an upgrade must restart the running service:\n{calls}"
    );
    assert!(
        !calls.contains("systemctl start"),
        "an upgrade must never start the service:\n{calls}"
    );
}

/// A purge removes the configuration; a plain removal keeps it.
///
/// dpkg leaves a `config|noreplace` file alone on `remove`, on
/// purpose: the operator's edited configuration has to survive a
/// reinstall.  It is `purge` that says "remove everything" — and the
/// configuration carries the admin password and the ACME account.
#[test]
fn the_postremove_removes_the_configuration_only_on_a_purge() {
    let sandbox = Sandbox::with_units(&["pangolin-ngx.service"]);
    let config = sandbox.root().join("etc/pangolin/ngx.yml");
    fs::create_dir_all(config.parent().expect("a parent")).expect("create the config dir");
    fs::write(&config, "admin:\n  password: secret\n").expect("write the config");

    let output = sandbox.run("postremove.sh", &["remove"]);
    assert!(
        output.status.success(),
        "the postremove must not fail:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        config.exists(),
        "a plain removal must keep the configuration"
    );

    let output = sandbox.run("postremove.sh", &["purge"]);
    assert!(
        output.status.success(),
        "the postremove must not fail on a purge:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!config.exists(), "a purge must remove the configuration");
}

/// The tunnel node on its own enables nothing at all.
///
/// A source host that runs its own web server installs the tunnel
/// node next to it; the gateway's unit is not even there.
#[test]
fn the_postinstall_enables_nothing_when_only_the_tunnel_node_is_installed() {
    let sandbox = Sandbox::with_units(&["pangolin-tun.service"]);
    let output = sandbox.run("postinstall.sh", &[]);

    assert!(
        output.status.success(),
        "the postinstall must not fail:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        sandbox.wants_link("pangolin-ngx.service").is_none(),
        "the gateway must not be enabled when it is not installed"
    );
    assert!(
        sandbox.wants_link("pangolin-tun.service").is_none(),
        "the tunnel node must not be enabled"
    );
}

/// An older installation is reported, and the install still succeeds.
///
/// The tarball and the one-line installer put the gateway into
/// `/usr/local/pangolin`.  The package uses a different layout
/// entirely, and moving the runtime state of one into the other is
/// not something an installer may do on its own — so the script
/// warns and leaves the data alone.
#[test]
fn the_postinstall_warns_about_an_older_installation_without_failing() {
    let sandbox = Sandbox::with_units(&["pangolin-ngx.service"]);
    let legacy = sandbox.root().join("usr/local/pangolin");
    fs::create_dir_all(&legacy).expect("create the legacy directory");
    fs::write(legacy.join("pangolin-ngx"), "#!/bin/sh\n").expect("write the legacy binary");

    let output = sandbox.run("postinstall.sh", &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "an older installation must not fail the install:\n{stdout}{stderr}"
    );
    assert!(
        stderr.contains("/usr/local/pangolin"),
        "the warning must name the old directory:\n{stderr}"
    );
    assert!(
        stderr.contains("systemctl stop"),
        "the warning must tell the operator to stop the old service:\n{stderr}"
    );
    assert!(
        legacy.join("pangolin-ngx").exists(),
        "the old installation must be left where it is"
    );
}

/// The system user is created once, with the `pangolin` group as its
/// primary group.
///
/// The group is named with `--gid`, not with `--user-group`:
/// `--user-group` asks `useradd` to *create* the group, and useradd
/// refuses when a group of that name already exists — which it does,
/// because the script creates it two lines earlier.  The install would
/// abort on every machine.
///
/// The user may already exist — the Ansible role and the one-line
/// installer create it too — and a re-install must not fail because
/// of that.
#[test]
fn the_preinstall_creates_the_system_user_once() {
    let sandbox = Sandbox::with_units(&[]);

    let output = sandbox.run("preinstall.sh", &[]);
    assert!(
        output.status.success(),
        "the preinstall must not fail:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let calls = sandbox.calls();
    assert!(
        calls.contains("groupadd --system pangolin"),
        "the group must be created:\n{calls}"
    );
    assert!(
        calls.contains("useradd --system --no-create-home --gid pangolin"),
        "the user must be created with the pangolin group as its primary group:\n{calls}"
    );
    assert!(
        !calls.contains("--user-group"),
        "`--user-group` asks useradd to create the group, and useradd refuses when it \
         already exists:\n{calls}"
    );
    assert!(
        calls.contains("--home-dir /var/lib/pangolin"),
        "the user's home has to be the state directory:\n{calls}"
    );
    assert!(
        calls.contains("--shell /usr/sbin/nologin"),
        "the service user must not be able to log in:\n{calls}"
    );
}

/// An existing `pangolin` user is left alone.
///
/// The Ansible role and the one-line installer create that user, so a
/// host that already has one is the normal case: the package has to
/// install on top of it, not against it.
#[test]
fn the_preinstall_leaves_an_existing_user_alone() {
    let sandbox = Sandbox::with_state(&[], true, true);

    let output = sandbox.run("preinstall.sh", &[]);
    assert!(
        output.status.success(),
        "the preinstall must not fail on a host that already has the user:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let calls = sandbox.calls();
    assert!(
        !calls.contains("groupadd"),
        "an existing group must not be created again:\n{calls}"
    );
    assert!(
        !calls.contains("useradd"),
        "an existing user must not be created again:\n{calls}"
    );
}

/// An upgrade does not stop the service.
///
/// dpkg calls `prerm` with `upgrade` and rpm's `%preun` with `1`; in
/// both cases the running service has to survive so the new package
/// can take over without an outage.
#[test]
fn the_preremove_leaves_the_service_running_during_an_upgrade() {
    let sandbox = Sandbox::with_units(&["pangolin-ngx.service"]);

    for argument in ["upgrade", "1"] {
        let output = sandbox.run("preremove.sh", &[argument]);
        assert!(
            output.status.success(),
            "the preremove must not fail:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !sandbox.calls().contains("systemctl stop"),
            "an upgrade ({argument}) must not stop the service:\n{}",
            sandbox.calls()
        );
    }

    // A real removal does stop it.
    let output = sandbox.run("preremove.sh", &["remove"]);
    assert!(
        output.status.success(),
        "the preremove must not fail:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        sandbox
            .calls()
            .contains("systemctl stop pangolin-ngx.service"),
        "a removal must stop the service:\n{}",
        sandbox.calls()
    );
    assert!(
        sandbox
            .calls()
            .contains("systemctl disable pangolin-ngx.service"),
        "a removal must take the service out of the boot sequence:\n{}",
        sandbox.calls()
    );
}

/// The runtime state survives an uninstall.
///
/// The database, the certificates and the logs are not in the package,
/// so the package manager has no business deleting them — and a
/// non-empty directory is left alone by dpkg anyway.
#[test]
fn the_postremove_reports_the_state_it_leaves_behind() {
    let sandbox = Sandbox::with_units(&["pangolin-ngx.service"]);
    let state = sandbox.root().join("var/lib/pangolin");
    fs::create_dir_all(&state).expect("create the state directory");
    fs::write(state.join("pangolin.db"), "sqlite").expect("write the database");

    let output = sandbox.run("postremove.sh", &["remove"]);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "the postremove must not fail:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        state.join("pangolin.db").exists(),
        "the database must survive the uninstall"
    );
    assert!(
        stdout.contains("/var/lib/pangolin"),
        "the postremove must name the directory it left behind:\n{stdout}"
    );
}
