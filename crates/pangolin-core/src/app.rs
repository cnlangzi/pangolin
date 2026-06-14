//! Application state — shared between gateway (ngx) and admin UI.
//!
//! Both `ngx` (the gateway binary) and `admin` (the UI library) use the same
//! `App` type from this shared crate.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::Connection;
use tokio::sync::{Mutex, RwLock};

use crate::tunnel::YamuxTunnel;
use crate::{
    EventBuffer, EventType, Indexes,
    config::Config,
    db,
    types::{ChallengeKind, ChallengeType, DnsProviderKind},
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
///
/// `effective_kind` (issue #55) is the concrete challenge kind the
/// order will use — `http-01` / `dns-01` / `dns-persist-01`. The
/// per-SAN `challenges` list carries the legacy
/// `pangolin_core::ChallengeType` (a 2-variant enum) for
/// compatibility with the existing match arms in `ngx::acme`, and
/// the `effective_kind` field is the source of truth for the
/// wire-level choice. `effective_kind` is always equal for every
/// SAN in the order — splitting the kind per SAN is not supported
/// (the IETF draft puts the wildcard and the bare base in the
/// same order and uses one TXT for both).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuancePlan {
    pub challenges: Vec<(String, ChallengeType)>,
    pub dns_provider_name: Option<String>,
    pub effective_kind: ChallengeKind,
}

/// Decide how to issue/renew a cert for a domain.
///
/// Pure function: no I/O, no async, no DB. The caller supplies the
/// current `DnsIndex` and the `Domain` row to make the decision.
///
/// Rules (issue #55 — supersedes the pre-#55 behaviour):
///   * `domain.auto_issue == false` → empty plan, no-op.
///   * The effective challenge kind is `domain.effective_challenge_kind(...)`:
///     explicit `challenge_kind` wins, otherwise the auto default is
///     `dns-01` if a DNS provider is linked, else `http-01`.
///   * All SANs in the order share the same kind (no per-SAN switching).
///   * Wildcard × http-01 → error containing "RFC 8555 §8.3".
///   * DNS-01 / dns-persist-01 with no DNS provider linked → error
///     pointing at the /dns admin page (scenario C).
///   * Wildcard × no-DNS-provider → error (no per-SAN workaround).
///   * FQDN with an unknown provider → error.
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
            effective_kind: ChallengeKind::Http01, // unused when challenges is empty
        });
    }
    if sans.is_empty() {
        return Err(PangolinError::Config(
            "plan_issuance called with empty SAN list".into(),
        ));
    }

    let any_wildcard = sans.iter().any(|s| s.starts_with("*."));

    // First pass: pick the DNS provider the WHOLE order will use
    // (whichever one any of the SANs is associated with, since
    // DNS-01 SANs in a single order all live on the same DNS
    // provider account). The match is done before per-SAN logic
    // so a wildcard can force every SAN in the same order onto
    // Dns01 — the per-domain lookup below would otherwise pick
    // Http01 for the base `yaitoo.cn` (which has no provider
    // attached in the DB) while the wildcard `*.yaitoo.cn` uses
    // Dns01, leaving the base unable to find a matching
    // http-01 challenge.
    let order_provider: Option<String> = sans.iter().find_map(|san| idx.lookup_dns(san).clone());
    if let Some(p) = &order_provider
        && !idx.providers.contains_key(p)
    {
        return Err(PangolinError::Config(format!(
            "order references unknown or disabled dns_provider '{p}'"
        )));
    }
    if any_wildcard && order_provider.is_none() {
        return Err(PangolinError::Config(
            "wildcard SAN in order requires a DNS provider (set dns_provider \
             on the wildcard domain or its base)"
                .into(),
        ));
    }

    // Resolve the effective kind once for the whole order. The
    // domain row's `challenge_kind` (or its auto default) is the
    // single source of truth — there is no per-SAN kind switching.
    let effective = domain.effective_challenge_kind(order_provider.is_some());

    // Wildcard × http-01 — rejected here (plan time) so the operator
    // sees a clear error before the ACME server refuses. The error
    // message MUST contain the literal string "RFC 8555 §8.3" — the
    // admin UI tests grep for it and operators can search the docs
    // for it.
    if any_wildcard && effective == ChallengeKind::Http01 {
        let wildcard = sans
            .iter()
            .find(|s| s.starts_with("*."))
            .cloned()
            .unwrap_or_default();
        return Err(PangolinError::Config(format!(
            "wildcard SAN '{wildcard}' in order cannot be validated with http-01 \
             (RFC 8555 §8.3 — ACME servers do not offer an http-01 \
             challenge for wildcard identifiers). \
             Set this domain's challenge_kind to 'dns-01' or \
             'dns-persist-01', or link a DNS provider to the base \
             domain so the auto-default resolves to dns-01."
        )));
    }

    // DNS-based challenge with no provider — scenario C. The error
    // tells the operator exactly where to go to fix it.
    let needs_dns_provider = matches!(
        effective,
        ChallengeKind::Dns01 | ChallengeKind::DnsPersist01
    );
    if needs_dns_provider && order_provider.is_none() {
        return Err(PangolinError::Config(format!(
            "domain '{domain}' is configured for {effective} but no DNS provider is linked \
             (neither this domain nor its base has a dns_provider set). \
             Add a DNS provider under the /dns admin page and link it to \
             this domain, or switch the domain to challenge_kind = 'http-01' \
             (http-01 is only valid for non-wildcard SANs per RFC 8555 §8.3).",
            domain = domain.domain,
            effective = effective,
        )));
    }

    let required_provider = if needs_dns_provider {
        // Unwrap is safe: we just checked `order_provider.is_none()`
        // and returned the error above.
        order_provider.clone()
    } else {
        None
    };

    // Build the per-SAN plan. All SANs use the same `effective` kind
    // — wildcard × http-01 was rejected above, and the IETF draft
    // uses one TXT for both the wildcard and the bare base in the
    // same order, so splitting the kind per SAN is not supported.
    //
    // The plan still uses `ChallengeType::Dns01` for both `Dns01`
    // and `DnsPersist01` — the wire-level distinction is made
    // later in `pick_and_setup_challenge`, which decides which
    // `instant_acme::ChallengeType` to request and which TXT
    // helper to invoke. The plan carries the kind through
    // `plan.dns_provider_name` + a separate `effective_kind` field
    // would be cleaner, but keeping it on the domain row is the
    // existing convention (issue #55 says: configuration lives on
    // the domain row).
    let ct = match effective {
        ChallengeKind::Http01 => ChallengeType::Http01,
        ChallengeKind::Dns01 | ChallengeKind::DnsPersist01 => ChallengeType::Dns01,
    };
    let challenges: Vec<(String, ChallengeType)> =
        sans.iter().map(|san| (san.clone(), ct)).collect();

    Ok(IssuancePlan {
        challenges,
        dns_provider_name: required_provider,
        effective_kind: effective,
    })
}

/// Cross-crate bridge from the admin UI to the ACME issuance pipeline
/// (issue #45). The retry route lives in `admin`, but the only code
/// that knows how to drive an issuance is `ngx::acme::AcmeState`, and
/// `admin` cannot depend on `ngx` (it would pull pingora + TLS into
/// the admin lib's compile tree). This trait is the seam: ngx
/// implements it on `AcmeState`, registers an `Arc<dyn CertRetrier>`
/// on the `App`, and admin's `POST /certs/retry` dispatches through it.
///
/// `retry` is fire-and-forget from the HTTP caller's perspective — it
/// returns once the issuance has finished (or errored). The HTTP
/// handler can choose to await it for sync UX or spawn it for async.
#[async_trait::async_trait]
pub trait CertRetrier: Send + Sync {
    /// Run a one-shot ACME attempt for `domain`. The implementor is
    /// expected to drive the status row (`Pending`/`Issuing`/…) via
    /// `db::set_cert_status_atomic` so the admin UI converges without
    /// the caller having to do its own bookkeeping.
    async fn retry(&self, domain: &str) -> anyhow::Result<()>;
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
    /// Active tunnel sessions: tun_name → yamux control handle.
    /// Holds the per-tun control plane used to open new yamux
    /// streams from ngx to that tun.
    pub tun_sessions: Arc<RwLock<std::collections::HashMap<String, YamuxTunnel>>>,
    /// Per-tun last-seen timestamp (Unix seconds), used by the
    /// admin UI's tun list. Updated by the tunnel accept loop
    /// on each new request.
    pub tun_last_seen: Arc<RwLock<std::collections::HashMap<String, i64>>>,
    /// TLS cert manager (ACME + manual upload)
    pub cert_manager: CertManager,
    /// In-memory event buffer for dashboard activity feed
    pub events: Arc<EventBuffer>,
    /// Wake-up channel: admin writes (DNS provider add/edit/delete, domain
    /// upsert that changes auto_issue or dns_provider) notify here. The
    /// `AcmeState` background loop subscribes and reloads on each tick.
    /// Cheap (just a permit); no shared state.
    pub dns_change_notify: Arc<tokio::sync::Notify>,
    /// Bridge from admin's `POST /certs/retry` to the ACME pipeline
    /// (issue #45). Wired by `ngx::main` after `AcmeState` is built.
    /// `None` in process modes that don't run the ACME service (e.g.
    /// admin-only unit tests), in which case the retry endpoint returns
    /// a 503 with a clear message.
    pub cert_retrier: RwLock<Option<Arc<dyn CertRetrier>>>,
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

        // Issue #45 follow-up: sweep any cert row left in `Issuing` by a
        // prior process that died mid-ACME-call (panic / OOM / SIGKILL).
        // The row would otherwise spin in `Issuing` until the next
        // renewal scan (default 6 hours), giving operators no
        // self-recovery path. 10 minutes is a safe ceiling on the
        // legitimate ACME wall clock: DNS-01 propagation (≤120s) +
        // order-ready poll (10×5s) + cert poll (30×5s) ≈ 5 minutes
        // worst case, doubled for slack.
        match db::recover_stuck_issuing_rows(&conn, chrono::Duration::minutes(10)) {
            Ok(swept) if !swept.is_empty() => {
                log::warn!(
                    "ACME startup watchdog: reset {} stuck Issuing row(s) to Failed: {}",
                    swept.len(),
                    swept.join(", ")
                );
            }
            Ok(_) => {}
            Err(e) => {
                log::warn!("ACME startup watchdog failed: {}", e);
            }
        }

        let sites = db::list_sites(&conn)?;
        let domains = db::list_domains(&conn)?;
        let providers = db::list_dns_providers(&conn)?;
        let indexes = Indexes::build(sites, domains.clone());
        let dns_index = DnsIndex::build(&providers, &domains);

        Ok(Self {
            db: Arc::new(Mutex::new(conn)),
            indexes: Arc::new(RwLock::new(indexes)),
            ws_path: config.tunnel.ws_path.clone(),
            dns_index: Arc::new(RwLock::new(dns_index)),
            config,
            tun_sessions: Arc::new(RwLock::new(std::collections::HashMap::new())),
            tun_last_seen: Arc::new(RwLock::new(std::collections::HashMap::new())),
            cert_manager,
            events: Arc::new(EventBuffer::new()),
            dns_change_notify: Arc::new(tokio::sync::Notify::new()),
            cert_retrier: RwLock::new(None),
        })
    }

    /// Install the [`CertRetrier`] bridge once the ACME pipeline is built.
    /// Called from `ngx::main` after `AcmeState::empty()` produces the
    /// concrete retrier. Idempotent — re-installing replaces the prior
    /// retrier (useful for tests that swap a real `AcmeState` for a
    /// fake during one process lifetime).
    pub async fn set_cert_retrier(&self, retrier: Arc<dyn CertRetrier>) {
        *self.cert_retrier.write().await = Some(retrier);
    }

    /// Reload indexes from DB. Called after every admin write operation.
    /// Also fires `dns_change_notify` so the `AcmeState` background loop
    /// picks up new DNS providers / domain associations.
    pub async fn reload_indexes(&self) {
        let conn = self.db.lock().await;
        let sites = db::list_sites(&conn).unwrap_or_default();
        let domains = db::list_domains(&conn).unwrap_or_default();
        let providers = db::list_dns_providers(&conn).unwrap_or_default();
        let indexes = Indexes::build(sites, domains.clone());
        let dns_index = DnsIndex::build(&providers, &domains);
        *self.indexes.write().await = indexes;
        *self.dns_index.write().await = dns_index;
        drop(conn);
        // Wake the AcmeState loop (if any) so it re-reads dns_providers.
        self.dns_change_notify.notify_one();
    }

    /// Register a live tunnel session. Called when a tun node connects via WS.
    pub async fn register_tun(&self, name: String, tunnel: YamuxTunnel) {
        self.tun_sessions.write().await.insert(name, tunnel);
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
///
/// As of issue #39, the tunnel carries raw HTTP/1.1 bytes inside
/// yamux streams, and HTTP/WS responses are matched by stream
/// (not by an rid). The data-plane control message of the old
/// msgpack protocol is gone; only the registration lifecycle
/// remains.
#[derive(Debug, Clone)]
pub struct TunnelMessage {
    /// Reserved for future per-stream control-plane messages.
    /// Currently unused — kept so external callers can still
    /// construct a placeholder value when interfacing with the
    /// admin UI, which expects a stable message type.
    pub _reserved: (),
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
    pub email: Option<String>,
    pub acme_directory: String,
    pub renew_threshold_days: u32,
    pub renew_check_interval_hours: u32,
    /// Private key type: "ecdsa" or "rsa".
    pub key_type: String,
}

impl CertManager {
    /// Create a new CertManager.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cert_dir: PathBuf,
        email: Option<String>,
        acme_directory: String,
        renew_threshold_days: u32,
        renew_check_interval_hours: u32,
        key_type: String,
    ) -> Self {
        Self {
            cert_dir,
            email,
            acme_directory,
            renew_threshold_days,
            renew_check_interval_hours,
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
            email: None,
            acme_directory: "https://acme-v02.api.letsencrypt.org/directory".into(),
            renew_threshold_days: 14,
            renew_check_interval_hours: 6,
            key_type: "ecdsa".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DnsProvider, DnsProviderKind, Domain};
    use chrono::Utc;
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
            challenge_kind: None,
            created_at: Utc::now(),
        }
    }

    /// Issue #55: build a domain with a specific `challenge_kind` for
    /// the new plan_issuance tests. The plain `make_domain` helper
    /// leaves the field at `None` (auto); this one sets it explicitly.
    fn make_domain_with_kind(
        d: &str,
        auto: bool,
        dns: Option<&str>,
        kind: Option<ChallengeKind>,
    ) -> Domain {
        Domain {
            domain: d.into(),
            site_name: "app".into(),
            enabled: true,
            auto_issue: auto,
            dns_provider: dns.map(String::from),
            challenge_kind: kind,
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
        assert!(
            plan.challenges
                .iter()
                .all(|(_, c)| *c == ChallengeType::Dns01)
        );
    }

    #[test]
    fn plan_mixed_dns_and_no_dns_per_identifier() {
        // SAN: ["foo.example.com", "bar.example.com"]. Only foo has DNS.
        //
        // After the wildcard-base fix, the planner uses an
        // "all-or-nothing" policy for Dns01: if ANY SAN in the
        // order has a DNS provider attached, the WHOLE order is
        // routed through that provider. This avoids the
        // wildcard+base mismatch (where the wildcard forces Dns01
        // and the bare base falls back to Http01, leaving the
        // base unable to find a matching http-01 challenge in
        // the authorization). For per-SAN choice we'd need to
        // either pick a single challenge type per order or
        // implement per-SAN challenge switching; both add
        // complexity for a marginal case.
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
                ("bar.example.com".into(), ChallengeType::Dns01),
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

    // ---- Issue #55: per-domain challenge_kind tests ----
    //
    // The pre-#55 behaviour was implicit (DNS-01 for any domain with a
    // DNS provider, HTTP-01 otherwise, dns-persist-01 for wildcards).
    // Post-#55 the domain row carries an explicit
    // `challenge_kind: Option<ChallengeKind>` that the planner honours
    // — the spec requires six scenarios below and a clear error
    // message containing "RFC 8555 §8.3" for the wildcard × http-01
    // case. These tests pin the behaviour so a future refactor
    // cannot silently change it.

    #[test]
    fn plan_explicit_http01_non_wildcard() {
        // Non-wildcard, no DNS provider, explicit http-01 — ok.
        let idx = DnsIndex::default();
        let d = make_domain_with_kind("example.com", true, None, Some(ChallengeKind::Http01));
        let plan = plan_issuance(&["example.com".into()], &d, &idx).unwrap();
        assert_eq!(plan.effective_kind, ChallengeKind::Http01);
        assert_eq!(
            plan.challenges,
            vec![("example.com".into(), ChallengeType::Http01)]
        );
        assert!(plan.dns_provider_name.is_none());
    }

    #[test]
    fn plan_explicit_dns01_with_provider() {
        // FQDN with linked DNS provider, explicit dns-01 — must use dns-01.
        let providers = vec![make_provider("cf", DnsProviderKind::Cloudflare)];
        let domains = vec![make_domain("example.com", true, Some("cf"))];
        let idx = DnsIndex::build(&providers, &domains);
        let d = make_domain_with_kind("example.com", true, Some("cf"), Some(ChallengeKind::Dns01));
        let plan = plan_issuance(&["example.com".into()], &d, &idx).unwrap();
        assert_eq!(plan.effective_kind, ChallengeKind::Dns01);
        assert_eq!(
            plan.challenges,
            vec![("example.com".into(), ChallengeType::Dns01)]
        );
        assert_eq!(plan.dns_provider_name.as_deref(), Some("cf"));
    }

    #[test]
    fn plan_explicit_dns_persist01_with_provider() {
        // FQDN with linked DNS provider, explicit dns-persist-01 — must
        // use dns-persist-01 (the plan's effective_kind is the source
        // of truth; the per-SAN ChallengeType stays Dns01 for
        // backwards-compat with the legacy match arms in ngx::acme).
        let providers = vec![make_provider("cf", DnsProviderKind::Cloudflare)];
        let domains = vec![make_domain("example.com", true, Some("cf"))];
        let idx = DnsIndex::build(&providers, &domains);
        let d = make_domain_with_kind(
            "example.com",
            true,
            Some("cf"),
            Some(ChallengeKind::DnsPersist01),
        );
        let plan = plan_issuance(&["example.com".into()], &d, &idx).unwrap();
        assert_eq!(plan.effective_kind, ChallengeKind::DnsPersist01);
        assert_eq!(plan.dns_provider_name.as_deref(), Some("cf"));
    }

    #[test]
    fn plan_explicit_http01_with_wildcard_rejected() {
        // Wildcard + explicit http-01 must be rejected with an error
        // message containing "RFC 8555 §8.3" (issue #55 verification
        // item + RFC 8555 §8.3 itself).
        let providers = vec![make_provider("cf", DnsProviderKind::Cloudflare)];
        let domains = vec![make_domain("example.com", true, Some("cf"))];
        let idx = DnsIndex::build(&providers, &domains);
        let d = make_domain_with_kind(
            "*.example.com",
            true,
            Some("cf"),
            Some(ChallengeKind::Http01),
        );
        let err = plan_issuance(&["*.example.com".into()], &d, &idx).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("RFC 8555 §8.3"),
            "error must contain the RFC reference, got: {msg}"
        );
    }

    #[test]
    fn plan_null_with_dns_provider_resolves_to_dns01() {
        // NULL challenge_kind + DNS provider linked — auto default is
        // dns-01 (NOT dns-persist-01, NOT http-01).
        let providers = vec![make_provider("cf", DnsProviderKind::Cloudflare)];
        let domains = vec![make_domain("example.com", true, Some("cf"))];
        let idx = DnsIndex::build(&providers, &domains);
        let d = make_domain_with_kind("example.com", true, Some("cf"), None);
        let plan = plan_issuance(&["example.com".into()], &d, &idx).unwrap();
        assert_eq!(plan.effective_kind, ChallengeKind::Dns01);
        assert_eq!(
            plan.challenges,
            vec![("example.com".into(), ChallengeType::Dns01)]
        );
    }

    #[test]
    fn plan_null_no_dns_provider_resolves_to_http01() {
        // NULL challenge_kind + no DNS provider — auto default is http-01.
        let idx = DnsIndex::default();
        let d = make_domain_with_kind("example.com", true, None, None);
        let plan = plan_issuance(&["example.com".into()], &d, &idx).unwrap();
        assert_eq!(plan.effective_kind, ChallengeKind::Http01);
        assert_eq!(
            plan.challenges,
            vec![("example.com".into(), ChallengeType::Http01)]
        );
    }

    #[test]
    fn plan_dns01_without_provider_errors_scenario_c() {
        // DNS-01 explicitly chosen but no DNS provider linked — must
        // error with a "scenario C" message telling the operator
        // exactly what to do (the error mentions /dns admin page so
        // they can fix it without reading the docs).
        let idx = DnsIndex::default();
        let d = make_domain_with_kind("example.com", true, None, Some(ChallengeKind::Dns01));
        let err = plan_issuance(&["example.com".into()], &d, &idx).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("DNS provider"), "got: {msg}");
        assert!(
            msg.contains("/dns"),
            "expected remediation hint, got: {msg}"
        );
    }

    #[test]
    fn plan_dns_persist01_without_provider_errors_scenario_c() {
        // Same as above for dns-persist-01.
        let idx = DnsIndex::default();
        let d = make_domain_with_kind("example.com", true, None, Some(ChallengeKind::DnsPersist01));
        let err = plan_issuance(&["example.com".into()], &d, &idx).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("DNS provider"), "got: {msg}");
    }

    #[test]
    fn plan_explicit_choice_overrides_dns_provider_link() {
        // FQDN with linked DNS provider but explicit http-01 — the
        // user's choice wins. No DNS provider is needed for the
        // order (http-01 is the only challenge).
        let providers = vec![make_provider("cf", DnsProviderKind::Cloudflare)];
        let domains = vec![make_domain("example.com", true, Some("cf"))];
        let idx = DnsIndex::build(&providers, &domains);
        let d = make_domain_with_kind("example.com", true, Some("cf"), Some(ChallengeKind::Http01));
        let plan = plan_issuance(&["example.com".into()], &d, &idx).unwrap();
        assert_eq!(plan.effective_kind, ChallengeKind::Http01);
        assert!(plan.dns_provider_name.is_none());
    }

    #[test]
    fn plan_challenge_kind_roundtrip_string() {
        // The on-disk form (lower-case, kebab-case) round-trips
        // through the public API. This pins the wire format so a
        // typo in `as_str` would break Pebble / admin tests.
        for k in ChallengeKind::ALL {
            let s = k.as_str();
            let parsed: ChallengeKind = s.parse().unwrap();
            assert_eq!(parsed, k, "roundtrip failed for {s}");
        }
    }

    #[test]
    fn domain_effective_challenge_kind_explicit_wins() {
        // `effective_challenge_kind` is a pure projection: explicit
        // `Some(_)` wins over the auto default. The function does
        // NOT enforce wildcard × http-01 (that lives in the planner
        // and the admin form).
        let d = make_domain_with_kind(
            "*.example.com",
            true,
            Some("cf"),
            Some(ChallengeKind::Http01),
        );
        // Even though this is a wildcard, the helper returns
        // Http01 because the caller asked for it. The planner
        // is the place that rejects this combination.
        assert_eq!(d.effective_challenge_kind(true), ChallengeKind::Http01);
    }

    #[test]
    fn domain_effective_challenge_kind_auto_with_dns() {
        let d = make_domain("example.com", true, Some("cf"));
        assert_eq!(d.effective_challenge_kind(true), ChallengeKind::Dns01);
    }

    #[test]
    fn domain_effective_challenge_kind_auto_without_dns() {
        let d = make_domain("example.com", true, None);
        assert_eq!(d.effective_challenge_kind(false), ChallengeKind::Http01);
    }

    #[test]
    fn domain_has_wildcard_san() {
        assert!(Domain::has_wildcard_san(&["*.example.com".into()]));
        assert!(Domain::has_wildcard_san(&[
            "example.com".into(),
            "*.example.com".into(),
        ]));
        assert!(!Domain::has_wildcard_san(&["example.com".into()]));
        assert!(!Domain::has_wildcard_san(&[]));
    }
}
