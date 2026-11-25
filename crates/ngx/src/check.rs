//! The offline configuration check (`pangolin-ngx --check`).
//!
//! The check answers one question: *would the gateway start with
//! this configuration?* It runs the validation steps of the real
//! startup and stops before anything is bound — no port is opened, no
//! service is started, no signal handler is installed and no PID file
//! is written.
//!
//! ## The steps
//!
//! 1. the crypto provider is installed (the process does it before
//!    the arguments are even parsed — rustls needs it),
//! 2. the configuration is parsed,
//! 3. the database is opened (`App::new`, which applies the pending
//!    migrations),
//! 4. the cert dir has to exist and has to be a directory,
//! 5. the certificates on disk are reconciled into the `certs` table,
//! 6. the SNI callback is built.
//!
//! ## What it is not
//!
//! The check is **not** a read-only operation. It opens the database
//! (which applies the pending migrations) and it reconciles the
//! certificates on disk into the `certs` table. That is deliberate:
//! a check that took a different path than the real startup would
//! answer a question nobody asked. The side effects are documented in
//! `--help` and in `docs/design/graceful-reload.md`.
//!
//! ## The cert dir
//!
//! The check **fails** when the cert dir is missing, and it does not
//! create it. The real startup creates the directory — a fresh
//! installation has none — but a check that creates what it is about
//! to complain about is no check at all. The operator creates the
//! directory (or points `acme.cert_dir` at an existing one) and runs
//! the check again.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pangolin_core::App;
use pangolin_core::config::Config;

/// Run the check against the configuration the gateway would start
/// with.
pub fn run_check(config: Config) -> anyhow::Result<()> {
    run_check_in(config, Path::new(crate::DB_PATH))
}

/// The check itself, with the database path spelled out.
///
/// The database path is a parameter so the unit tests can point it at
/// a throwaway file instead of the working directory.
fn run_check_in(config: Config, db_path: &Path) -> anyhow::Result<()> {
    // 3. The listen addresses. Every one of them is parsed by the
    // real startup, and every one of them is fatal when it does not
    // parse: `tunnel.addr` and `api.listen` by the gateway itself
    // (a clear error), the three pingora ones by pingora's listener
    // thread — which leaves the process alive and serving nothing.
    // A check that skipped them would answer "yes" for a
    // configuration the gateway refuses to start with.
    check_listen_addresses(&config)?;

    // 4. The database. `App::new` opens it and applies the pending
    // migrations — the same thing the real startup does.
    let app = Arc::new(App::new(
        db_path,
        config.clone(),
        crate::cert_manager(&config),
    )?);

    // 4. The cert dir. It has to be there and it has to be a
    // directory; the check does not create it.
    let cert_dir = PathBuf::from(&config.acme.cert_dir);
    if !cert_dir.exists() {
        anyhow::bail!(
            "the cert dir {} does not exist — create it, or point acme.cert_dir at an existing directory",
            cert_dir.display()
        );
    }
    if !cert_dir.is_dir() {
        anyhow::bail!("the cert dir {} is not a directory", cert_dir.display());
    }

    // 6. The reconciliation. It is the step that imports a
    // certificate issued while the gateway was down into the
    // `certs` table; a check that skipped it would not notice a
    // certificate that the disk and the database disagree about.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(crate::acme::scan_and_reconcile_blobs(&app))?;

    // 7. The SNI callback. Building it is what proves that the
    // certificate blobs the gateway will serve can be loaded.
    let _settings = crate::tls::build_sni_settings(cert_dir, app.clone())?;

    Ok(())
}

/// Check every listen address the gateway parses at startup.
///
/// Three shapes, because the startup parses them three ways:
///
/// * `tunnel.addr` and `api.listen` are parsed by the gateway itself
///   as a `SocketAddr` — a hostname is not a valid value there.
/// * `addr.http` and `addr.https` are parsed by pingora, which
///   resolves them with `to_socket_addrs()` and so accepts a
///   hostname. The check resolves them the same way, which is what
///   makes the two agree on what "usable" means.
/// * `admin.addr` is parsed by the admin listener of the gateway
///   itself (issue #261). The check resolves it with the very
///   function the listener resolves it with, so the two cannot drift
///   apart.
///
/// The "off" switches are the ones the gateway honours: `api.listen:
/// ":0"` and an empty `api.listen` switch the management API off, and
/// `addr.https: ":0"` (or empty) keeps the gateway from adding the
/// TLS listener at all — pingora never sees those. `addr.http` has
/// no such switch: the gateway hands it to pingora as it is, and
/// pingora cannot resolve `":0"` — its listener thread panics and the
/// process stays up serving nothing.
fn check_listen_addresses(config: &Config) -> anyhow::Result<()> {
    config.tunnel.addr.parse::<SocketAddr>().map_err(|e| {
        anyhow::anyhow!(
            "tunnel.addr is not a valid host:port address ({:?}): {e}",
            config.tunnel.addr
        )
    })?;

    if config.api.listen != ":0" && !config.api.listen.is_empty() {
        config.api.listen.parse::<SocketAddr>().map_err(|e| {
            anyhow::anyhow!(
                "api.listen is not a valid host:port address ({:?}): {e}",
                config.api.listen
            )
        })?;
    }

    resolve_listen_address("addr.http", &config.addr.http)?;

    // The admin listener resolves the address itself — the check
    // asks the very same function, so a hostname that the runtime
    // would accept is a hostname the check accepts, and an address
    // that does not resolve fails both.
    resolve_listen_address("admin.addr", &config.admin.addr)?;

    if config.addr.https != ":0" && !config.addr.https.is_empty() {
        resolve_listen_address("addr.https", &config.addr.https)?;
    }

    Ok(())
}

/// Resolve one of the listen addresses.
///
/// The resolution is the gateway's own — the same function the admin
/// listener uses — so the check and the runtime cannot disagree on
/// what "usable" means. A hostname is a valid value; one that does
/// not resolve is not.
fn resolve_listen_address(key: &str, value: &str) -> anyhow::Result<()> {
    crate::serve::resolve_listen_address(key, value).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A configuration that points at a real cert dir passes.
    ///
    /// The check is the operator's "can I start with this?" — a
    /// configuration the gateway would serve has to come out clean.
    #[test]
    fn a_valid_configuration_passes_the_check() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let cert_dir = dir.path().join("certs");
        std::fs::create_dir_all(&cert_dir).expect("create the cert dir");

        let mut config = Config::default();
        config.acme.cert_dir = cert_dir.display().to_string();

        let db_path = dir.path().join("pangolin.db");
        run_check_in(config, &db_path).expect("a valid configuration passes the check");
    }

    /// A listen address the gateway cannot parse fails the check.
    ///
    /// The real startup dies with `invalid tunnel server address`
    /// (or, for the pingora ones, leaves a process that serves
    /// nothing) — a check that answered "yes" here would send the
    /// operator into a handover that cannot work.
    #[test]
    fn an_unparsable_listen_address_fails_the_check() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let cert_dir = dir.path().join("certs");
        std::fs::create_dir_all(&cert_dir).expect("create the cert dir");
        let db_path = dir.path().join("pangolin.db");

        for (key, value) in [
            ("tunnel.addr", "not-an-address"),
            ("api.listen", "also-not-an-address"),
            ("addr.http", "http://127.0.0.1:8000"),
            ("admin.addr", "127.0.0.1:not-a-port"),
        ] {
            let mut config = Config::default();
            config.acme.cert_dir = cert_dir.display().to_string();
            match key {
                "tunnel.addr" => config.tunnel.addr = value.to_string(),
                "api.listen" => config.api.listen = value.to_string(),
                "addr.http" => config.addr.http = value.to_string(),
                "admin.addr" => config.admin.addr = value.to_string(),
                _ => unreachable!(),
            }

            let error = run_check_in(config, &db_path)
                .expect_err("an unparsable listen address must fail the check");
            assert!(
                error.to_string().contains(key),
                "the error has to name {key}: {error}"
            );
        }
    }

    /// The "do not listen" switches are valid addresses.
    ///
    /// `api.listen: ":0"` switches the management API off and
    /// `addr.https: ":0"` keeps the gateway from adding the TLS
    /// listener — neither is an error, and a check that refused them
    /// would send the operator chasing a problem that does not
    /// exist.
    ///
    /// `addr.http: ":0"` is **not** one of them: the gateway hands it
    /// to pingora, which cannot resolve it — its listener thread
    /// panics and the process stays up serving nothing. The check
    /// refuses it, and the documentation says so too.
    #[test]
    fn the_disabled_listeners_are_not_an_error() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let cert_dir = dir.path().join("certs");
        std::fs::create_dir_all(&cert_dir).expect("create the cert dir");

        let mut config = Config::default();
        config.acme.cert_dir = cert_dir.display().to_string();
        config.api.listen = ":0".to_string();
        config.addr.https = ":0".to_string();
        config.addr.http = "127.0.0.1:0".to_string();

        let db_path = dir.path().join("pangolin.db");
        run_check_in(config, &db_path).expect("the disabled listeners are not an error");
    }

    /// A hostname is a valid admin address.
    ///
    /// The regression this pins: the gateway resolves `admin.addr`
    /// itself now (issue #261), and a deployment that exposes the
    /// admin UI by name has to pass the check — the check and the
    /// runtime have to agree on what "usable" means.
    #[test]
    fn a_hostname_is_a_valid_admin_address() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let cert_dir = dir.path().join("certs");
        std::fs::create_dir_all(&cert_dir).expect("create the cert dir");

        let mut config = Config::default();
        config.acme.cert_dir = cert_dir.display().to_string();
        config.admin.addr = "localhost:9081".to_string();

        let db_path = dir.path().join("pangolin.db");
        run_check_in(config, &db_path).expect("a hostname is a valid admin address");
    }

    /// An admin address that does not resolve fails the check.
    ///
    /// The regression this pins: the same agreement as above — an
    /// address the runtime cannot resolve is an address the check has
    /// to refuse, and it has to name the key.
    #[test]
    fn an_unresolvable_admin_address_fails_the_check() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let cert_dir = dir.path().join("certs");
        std::fs::create_dir_all(&cert_dir).expect("create the cert dir");

        let mut config = Config::default();
        config.acme.cert_dir = cert_dir.display().to_string();
        config.admin.addr = "no-such-host.invalid:9081".to_string();

        let db_path = dir.path().join("pangolin.db");
        let error = run_check_in(config, &db_path)
            .expect_err("an admin address that does not resolve must fail the check");
        assert!(
            error.to_string().contains("admin.addr"),
            "the error has to name the key: {error}"
        );
    }

    /// A cert dir that is not there fails the check.
    ///
    /// The gateway would create the directory and start; the check
    /// refuses, because "the operator meant to point at a directory
    /// that exists" is the thing worth telling them.
    #[test]
    fn a_missing_cert_dir_fails_the_check() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let cert_dir = dir.path().join("certs");

        let mut config = Config::default();
        config.acme.cert_dir = cert_dir.display().to_string();

        let db_path = dir.path().join("pangolin.db");
        let error = run_check_in(config, &db_path).expect_err("a missing cert dir must fail");
        assert!(
            error.to_string().contains("cert dir"),
            "the error has to name the cert dir: {error}"
        );
    }

    /// The check does not create the cert dir it complained about.
    ///
    /// A check that creates the directory turns the operator's
    /// mistake into a silent success on the next run.
    #[test]
    fn the_check_does_not_create_the_cert_dir() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let cert_dir = dir.path().join("certs");

        let mut config = Config::default();
        config.acme.cert_dir = cert_dir.display().to_string();

        let db_path = dir.path().join("pangolin.db");
        let _ = run_check_in(config, &db_path);
        assert!(
            !cert_dir.exists(),
            "the check must not create the cert dir it complained about"
        );
    }

    /// A cert dir that is a file, not a directory, fails the check.
    ///
    /// The gateway would fail later — when it tries to write a
    /// certificate into it — so the check is the place that says so.
    #[test]
    fn a_cert_dir_that_is_a_file_fails_the_check() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let cert_dir = dir.path().join("certs");
        std::fs::write(&cert_dir, b"not a directory").expect("write a file");

        let mut config = Config::default();
        config.acme.cert_dir = cert_dir.display().to_string();

        let db_path = dir.path().join("pangolin.db");
        let error =
            run_check_in(config, &db_path).expect_err("a cert dir that is a file must fail");
        assert!(
            error.to_string().contains("cert dir"),
            "the error has to name the cert dir: {error}"
        );
    }
}
