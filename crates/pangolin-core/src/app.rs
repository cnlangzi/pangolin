//! Application state — shared between gateway (ngx) and admin UI.
//!
//! Both `ngx` (the gateway binary) and `admin` (the UI library) use the same
//! `App` type from this shared crate.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use rusqlite::Connection;
use tokio::sync::{mpsc, Mutex, RwLock};

use crate::{config::Config, db, Indexes};

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

/// TLS certificate manager supporting both ACME auto-renewal and manual upload.
pub struct CertManager {
    pub enabled: bool,
    pub cert_dir: PathBuf,
    pub email: String,
    pub acme_directory: String,
    pub renew_threshold_days: u32,
    pub renew_check_interval_hours: u32,
    pub renew_max_retries: u32,
}

impl CertManager {
    /// Resolve cert and key file paths for the given host.
    /// Searches in order: domain-specific dir under cert_dir/<host>/,
    /// then the default cert_dir root.
    pub fn resolve_cert(&self, host: &str) -> crate::Result<(String, String)> {
        // Try domain-specific cert under cert_dir/<host>/
        let host_dir = self.cert_dir.join(host);
        let cert = host_dir.join("fullchain.pem");
        let key = host_dir.join("privkey.pem");
        if cert.exists() && key.exists() {
            return Ok((
                cert.to_string_lossy().into_owned(),
                key.to_string_lossy().into_owned(),
            ));
        }
        // Fall back to default cert_dir root
        let cert = self.cert_dir.join("fullchain.pem");
        let key = self.cert_dir.join("privkey.pem");
        if cert.exists() && key.exists() {
            return Ok((
                cert.to_string_lossy().into_owned(),
                key.to_string_lossy().into_owned(),
            ));
        }
        Err(crate::PangolinError::Config(format!(
            "no certificate found for host {} (searched {}/ and {}/)",
            host,
            host_dir.display(),
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
            enabled: true,
            cert_dir: PathBuf::from("./certs"),
            email: String::new(),
            acme_directory: "https://acme-v02.api.letsencrypt.org/directory".into(),
            renew_threshold_days: 30,
            renew_check_interval_hours: 6,
            renew_max_retries: 3,
        }
    }
}
