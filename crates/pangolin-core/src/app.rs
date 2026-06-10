//! Application state — shared between gateway (ngx) and admin UI.
//!
//! Both `ngx` (the gateway binary) and `admin` (the UI library) use the same
//! `App` type from this shared crate.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use rusqlite::Connection;
use tokio::sync::{mpsc, Mutex, RwLock};

use crate::{config::Config, db, EventBuffer, EventType, Indexes};

/// Shared application state. Owned by `ngx` at runtime; `admin` receives it
/// via `Arc<App>` when handling HTTP requests.
pub struct App {
    /// SQLite connection (sync, protected by mutex for write ops; reads use own conn)
    pub db: Arc<Mutex<Connection>>,
    /// In-memory indexes rebuilt from DB on startup and after each admin write
    pub indexes: Arc<RwLock<Indexes>>,
    /// Global configuration
    pub config: Config,
    /// WebSocket path for tunnel registration (e.g. "/tunnel")
    pub ws_path: String,
    /// Active tunnel sessions: tun_name → sender channel
    pub tun_sessions: Arc<RwLock<std::collections::HashMap<String, mpsc::Sender<TunnelMessage>>>>,
    /// TLS cert manager (ACME + manual upload)
    pub cert_manager: CertManager,
    /// In-memory event buffer for dashboard activity feed
    pub events: Arc<EventBuffer>,
}

impl App {
    /// Open (or create) the SQLite database, run migrations, build indexes.
    pub fn new(
        db_path: impl AsRef<Path>,
        config: Config,
        cert_manager: CertManager,
    ) -> crate::Result<Self> {
        let conn = db::open(db_path.as_ref())?;
        db::migrate(&conn)?;

        let sites = db::list_sites(&conn)?;
        let domains = db::list_domains(&conn)?;
        let tokens = db::list_tokens(&conn)?;
        let now = Utc::now();
        let indexes = Indexes::build(sites, domains, &tokens, now);

        Ok(Self {
            db: Arc::new(Mutex::new(conn)),
            indexes: Arc::new(RwLock::new(indexes)),
            config,
            ws_path: "/tunnel".to_string(),
            tun_sessions: Arc::new(RwLock::new(std::collections::HashMap::new())),
            cert_manager,
            events: Arc::new(EventBuffer::new()),
        })
    }

    /// Reload indexes from DB. Called after every admin write operation.
    pub async fn reload_indexes(&self) {
        let conn = self.db.lock().await;
        let sites = db::list_sites(&conn).unwrap_or_default();
        let domains = db::list_domains(&conn).unwrap_or_default();
        let tokens = db::list_tokens(&conn).unwrap_or_default();
        let now = Utc::now();
        let indexes = Indexes::build(sites, domains, &tokens, now);
        *self.indexes.write().await = indexes;
    }

    /// Register a live tunnel session. Called when a tun node connects via WS.
    pub async fn register_tun(&self, name: String, sender: mpsc::Sender<TunnelMessage>) {
        self.tun_sessions.write().await.insert(name, sender);
    }

    /// Unregister a tunnel session. Called on WS disconnect.
    pub async fn unregister_tun(&self, name: &str) {
        self.tun_sessions.write().await.remove(name);
    }

    /// Add an event to the dashboard activity feed.
    pub fn add_event(&self, event: EventType) {
        self.events.push(crate::Event::new(event));
    }

    /// Get all events (newest first).
    pub fn get_events(&self) -> Vec<crate::Event> {
        self.events.get_all()
    }

    /// Get the most recent N events.
    pub fn get_recent_events(&self, n: usize) -> Vec<crate::Event> {
        self.events.get_recent(n)
    }
}

/// Message sent over a tunnel WebSocket from proxy to a remote tun node.
#[derive(Debug)]
pub struct TunnelMessage {
    /// Unique request ID to match response
    pub rid: String,
    /// Serialized TunnelRequestFrame msgpack bytes
    pub body: Vec<u8>,
    /// Response channel (filled by write_task when tun sends response frame)
    pub resp_tx: tokio::sync::oneshot::Sender<crate::types::TunnelResponseFrame>,
}

/// TLS certificate manager — disk blob layout + ACME issuance metadata.
///
/// In v2 there is no global "autorenew on/off" toggle. Auto-issuance is
/// controlled per-domain via `domains.auto_issue`; the global operational
/// tuning (cert_dir, renew threshold, etc.) is set via `[acme]` in pangolin.yml.
///
/// The `CertManager` itself is responsible for resolving on-disk cert blobs
/// to a (cert_path, key_path) pair at TLS handshake time. ACME renewal/issuance
/// orchestration lives in the `ngx` crate's `acme` module (PR-2 work).
pub struct CertManager {
    pub cert_dir: PathBuf,
    pub email: String,
    pub acme_directory: String,
    pub renew_threshold_days: u32,
    pub renew_check_interval_hours: u32,
    pub renew_max_retries: u32,
    /// Private key type: "ecdsa" or "rsa".
    pub key_type: String,
}

impl CertManager {
    /// Create a new CertManager.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cert_dir: PathBuf,
        email: String,
        acme_directory: String,
        renew_threshold_days: u32,
        renew_check_interval_hours: u32,
        renew_max_retries: u32,
        key_type: String,
    ) -> Self {
        Self {
            cert_dir,
            email,
            acme_directory,
            renew_threshold_days,
            renew_check_interval_hours,
            renew_max_retries,
            key_type,
        }
    }

    /// Resolve cert and key file paths for the given host.
    /// Search order for autocert blob layout:
    ///   1. cert_dir/{host}          (ECDSA blob)
    ///   2. cert_dir/{host}+rsa      (RSA blob)
    /// There is no `cert_dir/default` fallback — each host must have its own
    /// blob on disk; otherwise the SNI handshake for that host fails.
    /// Returns (blob_path, blob_path) — blob is a combined key+cert file.
    pub fn resolve_cert(&self, host: &str) -> crate::Result<(String, String)> {
        // Try ECDSA blob first
        let ecdsa_blob = self.cert_dir.join(host);
        if ecdsa_blob.exists() {
            return Ok((
                ecdsa_blob.to_string_lossy().into_owned(),
                ecdsa_blob.to_string_lossy().into_owned(),
            ));
        }
        // Try RSA blob
        let rsa_blob = self.cert_dir.join(format!("{}+rsa", host));
        if rsa_blob.exists() {
            return Ok((
                rsa_blob.to_string_lossy().into_owned(),
                rsa_blob.to_string_lossy().into_owned(),
            ));
        }
        Err(crate::PangolinError::Config(format!(
            "no certificate found for host {} (searched {}/ and {}/+rsa); \
             upload a cert or enable auto_issue on the domain",
            host,
            self.cert_dir.display(),
            self.cert_dir.display()
        )))
    }

    /// Issue or retrieve an existing cert for the given domain.
    /// Returns `(cert_path, key_path)`.
    pub fn get_or_issue_cert(&self, domain: &str) -> crate::Result<(PathBuf, PathBuf)> {
        let (cert_path, key_path) = self.resolve_cert(domain)?;
        log::info!(
            "using certificate for {}: cert={}, key={}",
            domain,
            cert_path,
            key_path
        );
        Ok((PathBuf::from(cert_path), PathBuf::from(key_path)))
    }
}

impl Default for CertManager {
    fn default() -> Self {
        Self {
            cert_dir: PathBuf::from("./certs"),
            email: String::new(),
            acme_directory: "https://acme-v02.api.letsencrypt.org/directory".into(),
            renew_threshold_days: 30,
            renew_check_interval_hours: 6,
            renew_max_retries: 3,
            key_type: "ecdsa".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn resolve_cert_prefers_ecdsa_over_rsa() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        fs::write(p.join("example.com"), "ecdsa-blob").unwrap();
        fs::write(p.join("example.com+rsa"), "rsa-blob").unwrap();
        let cm = CertManager {
            cert_dir: p.to_path_buf(),
            ..CertManager::default()
        };
        let (cert, key) = cm.resolve_cert("example.com").unwrap();
        assert!(cert.ends_with("example.com"));
        assert_eq!(cert, key);
    }

    #[test]
    fn resolve_cert_falls_back_to_rsa() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        fs::write(p.join("example.com+rsa"), "rsa-blob").unwrap();
        let cm = CertManager {
            cert_dir: p.to_path_buf(),
            ..CertManager::default()
        };
        let (cert, _) = cm.resolve_cert("example.com").unwrap();
        assert!(cert.ends_with("example.com+rsa"));
    }

    #[test]
    fn resolve_cert_fails_without_default_fallback() {
        // v2: no `default` blob fallback. A missing host must error out.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        fs::write(p.join("default"), "should-not-be-used").unwrap();
        let cm = CertManager {
            cert_dir: p.to_path_buf(),
            ..CertManager::default()
        };
        assert!(cm.resolve_cert("nope.example.com").is_err());
    }
}
