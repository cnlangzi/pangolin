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

/// TLS certificate manager supporting both ACME auto-renewal and manual upload.
pub struct CertManager {
    pub enabled: bool,
    pub cert_dir: PathBuf,
    pub email: String,
    pub acme_directory: String,
    pub renew_threshold_days: u32,
    pub renew_check_interval_hours: u32,
    pub renew_max_retries: u32,
    /// Runtime override for autorenew. If Some, overrides the config file setting.
    /// This allows dynamic toggling without restarting.
    runtime_autorenew_override: std::sync::Mutex<Option<bool>>,
}

impl CertManager {
    /// Create a new CertManager with the given settings.
    pub fn new(
        enabled: bool,
        cert_dir: PathBuf,
        email: String,
        acme_directory: String,
        renew_threshold_days: u32,
        renew_check_interval_hours: u32,
        renew_max_retries: u32,
    ) -> Self {
        Self {
            enabled,
            cert_dir,
            email,
            acme_directory,
            renew_threshold_days,
            renew_check_interval_hours,
            renew_max_retries,
            runtime_autorenew_override: std::sync::Mutex::new(None),
        }
    }

    /// Returns whether autorenew is currently enabled.
    /// Checks the runtime override first, then falls back to the config setting.
    pub fn is_autorenew_enabled(&self) -> bool {
        let override_val = *self.runtime_autorenew_override.lock().unwrap();
        override_val.unwrap_or(self.enabled)
    }

    /// Set the runtime autorenew override.
    /// Pass None to clear the override and use the config setting.
    pub fn set_autorenew_override(&self, enabled: Option<bool>) {
        *self.runtime_autorenew_override.lock().unwrap() = enabled;
    }

    /// Get the current autorenew override value, if set.
    /// Returns `Some(true)` if override is enabled, `Some(false)` if disabled, or `None` if no override is set.
    pub fn get_autorenew_setting(&self) -> Option<bool> {
        *self.runtime_autorenew_override.lock().unwrap()
    }

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
            runtime_autorenew_override: std::sync::Mutex::new(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cert_manager_autorenew_override_default() {
        let cm = CertManager::default();
        // Default: no override set, should use config value (enabled = true)
        assert!(cm.is_autorenew_enabled());
        assert!(cm.get_autorenew_setting().is_none());
    }

    #[test]
    fn cert_manager_autorenew_override_enabled() {
        let cm = CertManager::new(
            false, // disabled in config
            PathBuf::from("./certs"),
            String::new(),
            String::new(),
            30,
            6,
            3,
        );
        // Config says disabled, but we override to enable
        cm.set_autorenew_override(Some(true));
        assert!(cm.is_autorenew_enabled());
        assert_eq!(cm.get_autorenew_setting(), Some(true));
    }

    #[test]
    fn cert_manager_autorenew_override_disabled() {
        let cm = CertManager::new(
            true, // enabled in config
            PathBuf::from("./certs"),
            String::new(),
            String::new(),
            30,
            6,
            3,
        );
        // Config says enabled, but we override to disable
        cm.set_autorenew_override(Some(false));
        assert!(!cm.is_autorenew_enabled());
        assert_eq!(cm.get_autorenew_setting(), Some(false));
    }

    #[test]
    fn cert_manager_autorenew_override_cleared() {
        let cm = CertManager::new(
            true, // enabled in config
            PathBuf::from("./certs"),
            String::new(),
            String::new(),
            30,
            6,
            3,
        );
        // Override to disable
        cm.set_autorenew_override(Some(false));
        assert!(!cm.is_autorenew_enabled());
        // Clear override - should fall back to config (enabled)
        cm.set_autorenew_override(None);
        assert!(cm.is_autorenew_enabled());
        assert!(cm.get_autorenew_setting().is_none());
    }
}
