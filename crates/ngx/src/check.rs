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
    // 3. The database. `App::new` opens it and applies the pending
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

    // 5. The reconciliation. It is the step that imports a
    // certificate issued while the gateway was down into the
    // `certs` table; a check that skipped it would not notice a
    // certificate that the disk and the database disagree about.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(crate::acme::scan_and_reconcile_blobs(&app))?;

    // 6. The SNI callback. Building it is what proves that the
    // certificate blobs the gateway will serve can be loaded.
    let _settings = crate::tls::build_sni_settings(cert_dir, app.clone())?;

    Ok(())
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
