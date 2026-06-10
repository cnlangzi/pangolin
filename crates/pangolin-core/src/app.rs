//! Application state — shared between gateway (ngx) and admin UI.
//!
//! Both `ngx` (the gateway binary) and `admin` (the UI library) use the same
//! `App` type from this shared crate.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use rusqlite::Connection;
use tokio::sync::{mpsc, Mutex, RwLock};

use crate::{
    config::Config,
    db,
    types::{ChallengeType, DnsProviderKind},
    EventBuffer, EventType, Indexes,
};

/// In-memory index of DNS-related state, rebuilt from DB on startup and
/// after every admin write. The hot path (TLS handshake) does not read SQLite.
#[derive(Debug, Default, Clone)]
pub struct DnsIndex {
    /// DNS provider registry: name → kind. The actual `Arc<dyn DnsProvider>`
    /// instances are owned by the `ngx` crate's `AcmeState`; this index is
    /// the per-domain lookup table that the issuance pipeline consults.
    pub providers: HashMap<String, DnsProviderKind>,
    /// Per-domain DNS association: domain (FQDN or `*.` form) → provider name.
    /// Wildcard domains store under their `*.example.com` literal; sub-domains
    /// store under their FQDN.
    pub domain_dns: HashMap<String, String>,
}

impl DnsIndex {
    pub fn build(
        providers: &[crate::types::DnsProvider],
        domains: &[crate::types::Domain],
    ) -> Self {
        let providers: HashMap<String, DnsProviderKind> = providers
            .iter()
            .filter(|p| p.enabled)
            .map(|p| (p.name.clone(), p.kind))
            .collect();
        let domain_dns: HashMap<String, String> = domains
            .iter()
            .filter_map(|d| {
                d.dns_provider
                    .as_ref()
                    .map(|p| (d.domain.clone(), p.clone()))
            })
            .collect();
        Self {
            providers,
            domain_dns,
        }
    }

    /// Look up the DNS provider name for a given SAN identifier, FQDN-first
    /// then base (strip the leading label). Returns None if neither lookup
    /// has an association.
    pub fn lookup_dns(&self, lookup: &str) -> Option<String> {
        if let Some(p) = self.domain_dns.get(lookup) {
            return Some(p.clone());
        }
        // For a wildcard lookup like `*.example.com`, the "base" is
        // `example.com` (i.e. strip the `*.` prefix only). For a regular
        // FQDN like `foo.bar.example.com`, the base is `bar.example.com`
        // (strip the leading label).
        let base: &str = if let Some(rest) = lookup.strip_prefix("*.") {
            rest
        } else if let Some((_, rest)) = lookup.split_once('.') {
            rest
        } else {
            // No dot in lookup — can't form a base. (Single-label names
            // are not valid for ACME anyway.)
            return None;
        };
        if base == lookup {
            return None;
        }
        self.domain_dns.get(base).cloned()
    }
}

/// Plan for issuing/renewing a cert. Each SAN identifier gets its own
/// challenge type (DNS-01 for wildcards, DNS-01 for FQDN with a DNS
/// association, HTTP-01 otherwise). An empty `challenges` vec means
/// "do nothing" (the domain has `auto_issue = false`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuancePlan {
    pub challenges: Vec<(String, ChallengeType)>,
    pub dns_provider_name: Option<String>,
}

/// Decide how to issue/renew a cert for a domain.
///
/// Pure function: no I/O, no async, no DB. The caller supplies the
/// current `DnsIndex` and the `Domain` row to make the decision.
///
/// Rules (v2 design, locked 2026-06-10):
///   * `domain.auto_issue == false` → empty plan, no-op.
///   * Wildcard identifier without an associated DNS provider → error.
///   * FQDN identifier with an associated DNS provider → DNS-01.
///   * FQDN identifier without an associated DNS provider → HTTP-01.
///   * FQDN identifier whose association points to an unknown provider → error.
pub fn plan_issuance(
    sans: &[String],
    domain: &crate::types::Domain,
    idx: &DnsIndex,
) -> crate::Result<IssuancePlan> {
    use crate::PangolinError;

    if !domain.auto_issue {
        return Ok(IssuancePlan {
            challenges: vec![],
            dns_provider_name: None,
        });
    }
    if sans.is_empty() {
        return Err(PangolinError::Config(
            "plan_issuance called with empty SAN list".into(),
        ));
    }

    let mut challenges = Vec::with_capacity(sans.len());
    let mut required_provider: Option<String> = None;

    for san in sans {
        let is_wildcard = san.starts_with("*.");
        let associated = idx.lookup_dns(san);

        match (is_wildcard, associated) {
            (true, None) => {
                return Err(PangolinError::Config(format!(
                    "wildcard {san} requires DNS-01 but no dns_provider is associated \
                     with the FQDN or base domain"
                )));
            }
            (true, Some(p)) | (false, Some(p)) => {
                if !idx.providers.contains_key(&p) {
                    return Err(PangolinError::Config(format!(
                        "{san} references unknown or disabled dns_provider '{p}'"
                    )));
                }
                required_provider = Some(p);
                challenges.push((san.clone(), ChallengeType::Dns01));
            }
            (false, None) => {
                challenges.push((san.clone(), ChallengeType::Http01));
            }
        }
    }

    Ok(IssuancePlan {
        challenges,
        dns_provider_name: required_provider,
    })
}

/// Shared application state. Owned by `ngx` at runtime; `admin` receives it
/// via `Arc<App>` when handling HTTP requests.
pub struct App {
    /// SQLite connection (sync, protected by mutex for write ops; reads use own conn)
    pub db: Arc<Mutex<Connection>>,
    /// In-memory indexes rebuilt from DB on startup and after each admin write
    pub indexes: Arc<RwLock<Indexes>>,
    /// DNS provider + per-domain association index, rebuilt alongside `indexes`
    pub dns_index: Arc<RwLock<DnsIndex>>,
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
    /// Wake-up channel: admin writes (DNS provider add/edit/delete, domain
    /// upsert that changes auto_issue or dns_provider) notify here. The
    /// `AcmeState` background loop subscribes and reloads on each tick.
    /// Cheap (just a permit); no shared state.
    pub dns_change_notify: Arc<tokio::sync::Notify>,
}

impl App {
    /// Open (or create) the SQLite database, run migrations, build indexes.
    pub fn new(
        db_path: impl AsRef<Path>,
        config: Config,
        cert_manager: CertManager,
    ) -> crate::Result<Self> {
        let mut conn = db::open(db_path.as_ref())?;
        db::migrate(&mut conn)?;

        let sites = db::list_sites(&conn)?;
        let domains = db::list_domains(&conn)?;
        let tokens = db::list_tokens(&conn)?;
        let providers = db::list_dns_providers(&conn)?;
        let now = Utc::now();
        let indexes = Indexes::build(sites, domains.clone(), &tokens, now);
        let dns_index = DnsIndex::build(&providers, &domains);

        Ok(Self {
            db: Arc::new(Mutex::new(conn)),
            indexes: Arc::new(RwLock::new(indexes)),
            dns_index: Arc::new(RwLock::new(dns_index)),
            config,
            ws_path: "/tunnel".to_string(),
            tun_sessions: Arc::new(RwLock::new(std::collections::HashMap::new())),
            cert_manager,
            events: Arc::new(EventBuffer::new()),
            dns_change_notify: Arc::new(tokio::sync::Notify::new()),
        })
    }

    /// Reload indexes from DB. Called after every admin write operation.
    /// Also fires `dns_change_notify` so the `AcmeState` background loop
    /// picks up new DNS providers / domain associations.
    pub async fn reload_indexes(&self) {
        let conn = self.db.lock().await;
        let sites = db::list_sites(&conn).unwrap_or_default();
        let domains = db::list_domains(&conn).unwrap_or_default();
        let tokens = db::list_tokens(&conn).unwrap_or_default();
        let providers = db::list_dns_providers(&conn).unwrap_or_default();
        let now = Utc::now();
        let indexes = Indexes::build(sites, domains.clone(), &tokens, now);
        let dns_index = DnsIndex::build(&providers, &domains);
        *self.indexes.write().await = indexes;
        *self.dns_index.write().await = dns_index;
        drop(conn);
        // Wake the AcmeState loop (if any) so it re-reads dns_providers.
        self.dns_change_notify.notify_one();
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
    ///
    /// Search order for autocert blob layout:
    ///   1. `cert_dir/{host}` (ECDSA blob)
    ///   2. `cert_dir/{host}+rsa` (RSA blob)
    ///
    /// There is no `cert_dir/default` fallback — each host must have its own
    /// blob on disk; otherwise the SNI handshake for that host fails.
    /// Returns `(blob_path, blob_path)` — the blob is a combined key+cert file.
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
    use crate::types::{DnsProvider, DnsProviderKind, Domain};
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

    // ---- DnsIndex / plan_issuance tests ----

    fn make_provider(name: &str, kind: DnsProviderKind) -> DnsProvider {
        DnsProvider {
            name: name.into(),
            kind,
            enabled: true,
            config: "{}".into(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn make_domain(d: &str, auto: bool, dns: Option<&str>) -> Domain {
        Domain {
            domain: d.into(),
            site_name: "app".into(),
            enabled: true,
            auto_issue: auto,
            dns_provider: dns.map(String::from),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn dns_index_lookup_fqdn_first() {
        let providers = vec![make_provider("cf", DnsProviderKind::Cloudflare)];
        let domains = vec![make_domain("example.com", true, Some("cf"))];
        let idx = DnsIndex::build(&providers, &domains);
        assert_eq!(idx.lookup_dns("example.com").as_deref(), Some("cf"));
    }

    #[test]
    fn dns_index_lookup_falls_back_to_base() {
        let providers = vec![make_provider("cf", DnsProviderKind::Cloudflare)];
        let domains = vec![make_domain("example.com", true, Some("cf"))];
        let idx = DnsIndex::build(&providers, &domains);
        // foo.example.com → no FQDN hit → strip to example.com → hit.
        assert_eq!(idx.lookup_dns("foo.example.com").as_deref(), Some("cf"));
    }

    #[test]
    fn dns_index_lookup_wildcard_via_base() {
        let providers = vec![make_provider("cf", DnsProviderKind::Cloudflare)];
        let domains = vec![make_domain("example.com", true, Some("cf"))];
        let idx = DnsIndex::build(&providers, &domains);
        // No row keyed at `*.example.com` — but the base lookup hits.
        assert_eq!(idx.lookup_dns("*.example.com").as_deref(), Some("cf"));
    }

    #[test]
    fn dns_index_lookup_wildcard_exact_wins() {
        let providers = vec![
            make_provider("cf", DnsProviderKind::Cloudflare),
            make_provider("ali", DnsProviderKind::Aliyun),
        ];
        let domains = vec![
            make_domain("example.com", true, Some("cf")),
            make_domain("*.example.com", true, Some("ali")),
        ];
        let idx = DnsIndex::build(&providers, &domains);
        assert_eq!(idx.lookup_dns("*.example.com").as_deref(), Some("ali"));
    }

    #[test]
    fn dns_index_lookup_misses_for_unrelated() {
        let providers = vec![make_provider("cf", DnsProviderKind::Cloudflare)];
        let domains = vec![make_domain("example.com", true, Some("cf"))];
        let idx = DnsIndex::build(&providers, &domains);
        assert!(idx.lookup_dns("foo.bar.com").is_none());
    }

    #[test]
    fn plan_noop_when_auto_issue_false() {
        let idx = DnsIndex::default();
        let d = make_domain("example.com", false, None);
        let plan = plan_issuance(&["example.com".into()], &d, &idx).unwrap();
        assert!(plan.challenges.is_empty());
        assert!(plan.dns_provider_name.is_none());
    }

    #[test]
    fn plan_base_uses_http01_when_no_dns_association() {
        let idx = DnsIndex::default();
        let d = make_domain("example.com", true, None);
        let plan = plan_issuance(&["example.com".into()], &d, &idx).unwrap();
        assert_eq!(
            plan.challenges,
            vec![("example.com".into(), ChallengeType::Http01)]
        );
    }

    #[test]
    fn plan_base_uses_dns01_when_associated() {
        let providers = vec![make_provider("cf", DnsProviderKind::Cloudflare)];
        let domains = vec![make_domain("example.com", true, Some("cf"))];
        let idx = DnsIndex::build(&providers, &domains);
        let d = make_domain("foo.example.com", true, None); // lookup will fall back to base
        let plan = plan_issuance(&["foo.example.com".into()], &d, &idx).unwrap();
        assert_eq!(
            plan.challenges,
            vec![("foo.example.com".into(), ChallengeType::Dns01)]
        );
        assert_eq!(plan.dns_provider_name.as_deref(), Some("cf"));
    }

    #[test]
    fn plan_wildcard_must_have_dns() {
        let idx = DnsIndex::default();
        let d = make_domain("*.example.com", true, None);
        let err = plan_issuance(&["*.example.com".into()], &d, &idx).unwrap_err();
        assert!(err.to_string().contains("wildcard"), "{err}");
    }

    #[test]
    fn plan_mixed_san_list_per_identifier() {
        // Wildcard cert: SAN list is ["example.com", "*.example.com"].
        // example.com is associated with CF → DNS-01; *.example.com falls
        // back to the same base → DNS-01.
        let providers = vec![make_provider("cf", DnsProviderKind::Cloudflare)];
        let domains = vec![make_domain("example.com", true, Some("cf"))];
        let idx = DnsIndex::build(&providers, &domains);
        let d = make_domain("*.example.com", true, None);
        let plan =
            plan_issuance(&["example.com".into(), "*.example.com".into()], &d, &idx).unwrap();
        assert_eq!(plan.challenges.len(), 2);
        assert!(plan
            .challenges
            .iter()
            .all(|(_, c)| *c == ChallengeType::Dns01));
    }

    #[test]
    fn plan_mixed_dns_and_no_dns_per_identifier() {
        // SAN: ["foo.example.com", "bar.example.com"]. Only foo has DNS.
        let providers = vec![make_provider("cf", DnsProviderKind::Cloudflare)];
        let domains = vec![make_domain("foo.example.com", true, Some("cf"))];
        let idx = DnsIndex::build(&providers, &domains);
        let d = make_domain("foo.example.com", true, Some("cf"));
        let plan = plan_issuance(
            &["foo.example.com".into(), "bar.example.com".into()],
            &d,
            &idx,
        )
        .unwrap();
        assert_eq!(
            plan.challenges,
            vec![
                ("foo.example.com".into(), ChallengeType::Dns01),
                ("bar.example.com".into(), ChallengeType::Http01),
            ]
        );
    }

    #[test]
    fn plan_unknown_provider_errors() {
        // domain.dns_provider points to a name that no longer exists.
        let providers: Vec<DnsProvider> = vec![];
        let domains = vec![make_domain("example.com", true, Some("ghost"))];
        let idx = DnsIndex::build(&providers, &domains);
        let d = make_domain("example.com", true, Some("ghost"));
        let err = plan_issuance(&["example.com".into()], &d, &idx).unwrap_err();
        assert!(err.to_string().contains("unknown"), "{err}");
    }
}
