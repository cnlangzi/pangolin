//! Pangolin gateway (ngx) — public-facing HTTP/WebSocket proxy.
//!
//! Two services share the same `Arc<App>`:
//!   - HTTP proxy via `http_proxy_service` + `impl ProxyHttp` for domain-routed proxying
//!   - HTTP server via `HttpServer::new_app` + `impl ServeHttp` for admin API + static files

mod acme;
mod admin;
mod proxy;
mod serve;
mod tunnel;

pub use proxy::AppProxy;
pub use serve::AppHttp;

use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use pingora::apps::http_app::HttpServer;
use pingora::proxy::http_proxy_service;
use pingora::server::Server;
use pingora::services::listening::Service;
use rusqlite::Connection;
use tokio::sync::{mpsc, Mutex, RwLock};

use pangolin_core::{config::Config, db, Indexes};

/// The shared application state for both the proxy and the HTTP server.
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
        db_path: &PathBuf,
        config: Config,
        cert_manager: CertManager,
    ) -> anyhow::Result<Self> {
        let conn = db::open(db_path)?;
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
}

/// Message sent over a tunnel WebSocket to the remote tun node.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct TunnelMessage {
    /// Unique request ID to match response
    pub rid: String,
    /// Serialized HTTP request headers + path + method
    pub body: Vec<u8>,
    /// Whether this is the last chunk
    pub last: bool,
}

// ---- Tunnel session management ----

impl App {
    /// Register a live tunnel session. Called when a tun node connects via WS.
    pub async fn register_tun(&self, name: String, sender: mpsc::Sender<TunnelMessage>) {
        self.tun_sessions.write().await.insert(name, sender);
    }

    /// Unregister a tunnel session. Called on WS disconnect.
    pub async fn unregister_tun(&self, name: &str) {
        self.tun_sessions.write().await.remove(name);
    }
}

// ---- Cert manager ----

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
    pub fn resolve_cert(&self, host: &str) -> anyhow::Result<(String, String)> {
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
        anyhow::bail!(
            "no certificate found for host {} (searched {}/ and {}/)",
            host,
            host_dir.display(),
            self.cert_dir.display()
        )
    }

    /// Issue or retrieve an existing cert for the given domain.
    /// Returns `(cert_path, key_path)`.
    pub fn get_or_issue_cert(&self, domain: &str) -> anyhow::Result<(PathBuf, PathBuf)> {
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

// ---- CLI entry point ----

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "ngx")]
#[command(about = "Pangolin gateway — public-facing HTTP/WebSocket proxy")]
struct Args {
    /// Path to config file (default: ./pangolin.toml)
    #[arg(short, long, default_value = "pangolin.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = Args::parse();
    let config =
        Config::from_file(&args.config).map_err(|e| anyhow::anyhow!("config error: {}", e))?;

    let db_path = PathBuf::from("pangolin.db");
    let cert_manager = CertManager {
        enabled: config.cert.autorenew,
        cert_dir: PathBuf::from(&config.cert.cert_dir),
        email: config.cert.email.clone(),
        acme_directory: config.cert.acme_directory.clone(),
        renew_threshold_days: config.cert.renew_threshold_days,
        renew_check_interval_hours: config.cert.renew_check_interval_hours,
        renew_max_retries: config.cert.renew_max_retries,
    };
    let app = Arc::new(App::new(&db_path, config.clone(), cert_manager)?);

    log::info!(
        "Pangolin ngx {} starting on port {}",
        pangolin_core::VERSION,
        config.server.port
    );

    // Build pingora server
    let mut server = Server::new(None)?;
    server.bootstrap();

    let conf = server.configuration.clone();
    let app_proxy = AppProxy { app: app.clone() };
    let app_http = AppHttp { app: app.clone() };

    // HTTP proxy service (domain-routed)
    let mut proxy_service = http_proxy_service(&conf, app_proxy);
    proxy_service.add_tcp(&format!("0.0.0.0:{}", config.server.port));
    if config.server.tls_port > 0 {
        let tls_addr = format!("0.0.0.0:{}", config.server.tls_port);
        let host = config.server.host.as_deref().unwrap_or("default");
        let (cert_path, key_path) = app.cert_manager.resolve_cert(host)?;
        proxy_service.add_tls(&tls_addr, &cert_path, &key_path)?;
        log::info!("TLS enabled on {}", tls_addr);
    }
    server.add_service(proxy_service);

    // HTTP server (admin API + static files)
    let http_server: Service<_> =
        Service::new("pangolin-http".to_string(), HttpServer::new_app(app_http));
    server.add_service(http_server);

    // Tunnel WebSocket server (independent TCP listener, runs as background task)
    let app_tunnel = app.clone();
    let tunnel_addr = format!("127.0.0.1:{}", config.server.tunnel_port);
    tokio::spawn(async move {
        tunnel::start_tunnel_server(app_tunnel, &tunnel_addr).await;
    });

    server.run_forever();
}
