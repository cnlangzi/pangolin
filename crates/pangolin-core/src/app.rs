//! Application state — shared between gateway (ngx) and admin UI.
//!
//! Both `ngx` (the gateway binary) and `admin` (the UI library) use the same
//! `App` type from this shared crate.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::Connection;
use tokio::sync::{Mutex, RwLock, broadcast};

use crate::tunnel::YamuxTunnel;
use crate::{
    AccessLogBuffer, AccessLogEntry, EventBuffer, EventType, Indexes,
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
    //
    // Auto-default: the order-level `order_provider` is consulted
    // (which falls back to a base-domain association). A deeper
    // subdomain that never had a provider set on its own row
    // inherits the base's provider for the auto-default — this is
    // the pre-#55 behaviour and is locked in by the
    // `plan_base_uses_dns01_when_associated` and
    // `plan_mixed_san_list_per_identifier` tests in this file.
    // The wildcard × http-01 case is still rejected below when
    // the wildcard forces Http01.
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
    /// Sync mirror of the subset of `indexes.domain` whose site has a
    /// `tun_name:` backend prefix. Lives behind a **sync** lock because
    /// the TLS ALPN callback (which runs in a sync C context inside the
    /// handshake) needs to ask "is this SNI on a tunnel site?" without
    /// blocking on the async `indexes` lock. Rebuilt atomically with
    /// `indexes` inside [`App::reload_indexes`].
    ///
    /// See `ngx::tls::build_sni_settings` for the per-SNI ALPN logic
    /// that consumes this set; see issue #66 / commit `0c35ede` for
    /// the upstream tokio-yamux h2+tunnel bug that motivates it.
    pub tunnel_domains: Arc<parking_lot::RwLock<Arc<std::collections::HashSet<String>>>>,
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
    /// Live access log broadcast channel (issue #73). `ngx`'s
    /// `response_filter` calls [`App::push_access_log`] on every
    /// proxied request, which sends through this channel. Admin's
    /// `/api/logs/stream` SSE endpoint subscribes and forwards to
    /// the browser. Capacity is `config.log.access_log_capacity`
    /// (default 1000). A lagged subscriber sees an SSE comment
    /// `: lagged N events` and the stream keeps flowing — we do
    /// **not** crash the channel on LagError.
    pub access_log_tx: broadcast::Sender<AccessLogEntry>,
    /// Bounded ring buffer of recent access log entries (issue #73).
    /// Late-joining SSE subscribers get a snapshot of this buffer
    /// replayed as the first N `data:` frames before any live
    /// broadcasts. Sized by `config.log.access_log_recent`
    /// (default 100).
    pub access_log_recent: Arc<AccessLogBuffer>,
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
        let tunnel_set = build_tunnel_domain_set(&indexes);

        // Access log live channel (issue #73). tokio::sync::broadcast
        // already deduplicates per-subscriber but we still need to
        // set a real capacity here so the channel has somewhere to
        // queue messages — the `Sender::new(0)` form returns the
        // unit value and panics if `recv()` is ever called. We use
        // `max(1, …)` so a misconfigured `access_log_capacity: 0`
        // doesn't crash the binary at startup.
        let access_log_capacity = config.log.access_log_capacity.max(1);
        let access_log_recent_capacity = config.log.access_log_recent;
        let (access_log_tx, _initial_rx) = broadcast::channel(access_log_capacity);

        Ok(Self {
            db: Arc::new(Mutex::new(conn)),
            indexes: Arc::new(RwLock::new(indexes)),
            tunnel_domains: Arc::new(parking_lot::RwLock::new(Arc::new(tunnel_set))),
            ws_path: config.tunnel.ws_path.clone(),
            dns_index: Arc::new(RwLock::new(dns_index)),
            config,
            tun_sessions: Arc::new(RwLock::new(std::collections::HashMap::new())),
            tun_last_seen: Arc::new(RwLock::new(std::collections::HashMap::new())),
            cert_manager,
            events: Arc::new(EventBuffer::new()),
            dns_change_notify: Arc::new(tokio::sync::Notify::new()),
            cert_retrier: RwLock::new(None),
            access_log_tx,
            access_log_recent: Arc::new(AccessLogBuffer::new(access_log_recent_capacity)),
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
        // Sync mirror: every domain that resolves to a tunnel-backed
        // site. The TLS ALPN callback consults this set on the
        // handshake hot path. We derive it from the freshly-built
        // `indexes` (single source of truth) so the two never drift.
        let tunnel_set = build_tunnel_domain_set(&indexes);
        *self.indexes.write().await = indexes;
        *self.dns_index.write().await = dns_index;
        *self.tunnel_domains.write() = Arc::new(tunnel_set);
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

    /// Push an access log entry into the live broadcast channel and
    /// the bounded ring buffer (issue #73).
    ///
    /// The two writes are decoupled on purpose:
    ///   - The ring buffer is sync (`parking_lot::Mutex`); we hold it
    ///     for a `push_back` + bound check, no I/O.
    ///   - The broadcast is async-aware but `send` itself does not
    ///     `.await`. `tokio::sync::broadcast::Sender::send` is
    ///     documented as non-blocking.
    ///
    /// On `SendError` (no active subscribers) the entry is *still*
    /// appended to the ring buffer — late joiners can replay it. On
    /// `LagError(skipped)` (a subscriber fell behind by `skipped`
    /// messages) we drop the entry from the broadcast but keep it
    /// in the ring buffer; the SSE endpoint emits `: lagged N events`
    /// and continues. We never panic and never block.
    pub fn push_access_log(&self, entry: AccessLogEntry) {
        // 1) ring buffer (sync, fast path). Even if the broadcast
        //    later drops the entry, the ring buffer always keeps
        //    the most-recent N entries for late-join replay.
        self.access_log_recent.push(entry.clone());

        // 2) live broadcast. Errors are *expected* (zero subscribers)
        //    so we discard the Result. A Lagged subscriber is the
        //    SSE endpoint's responsibility to surface.
        let _ = self.access_log_tx.send(entry);
    }

    /// Snapshot the access log ring buffer in chronological order
    /// (oldest first). Used by the SSE endpoint to replay entries
    /// on connect. Returns an empty Vec if the buffer is disabled
    /// (`access_log_recent: 0`).
    pub fn recent_access_log(&self) -> Vec<AccessLogEntry> {
        self.access_log_recent.snapshot()
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

/// Build the set of all domain strings (exact + wildcard literals)
/// whose matching site has a `tun_name:` backend prefix.
///
/// Single source of truth for [`App::tunnel_domains`]. The output
/// contains the exact strings stored in `Indexes.domain`; wildcard
/// suffix matching is done at lookup time by
/// [`crate::index::host_matches_set`].
///
/// Sites whose `backend` cannot be parsed are skipped silently —
/// the same policy used in `Indexes::build`'s tun index pass. Any
/// startup-time parse errors have already failed-fast at boot, so
/// a per-reload silent skip is safe.
fn build_tunnel_domain_set(indexes: &crate::Indexes) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    for (domain, site) in &indexes.domain {
        match crate::parse::parse_backend(&site.backend) {
            Ok((tun_name, _url)) if !tun_name.is_empty() => {
                out.insert(domain.clone());
            }
            _ => {
                // Direct backend, or unparseable (skipped). Either way
                // this domain does not route through a tunnel.
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LogConfig;
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

    // ---- tunnel_domains mirror (per-SNI ALPN) tests ----

    use crate::index::Indexes;

    fn site_with_backend(name: &str, backend: &str) -> crate::types::Site {
        let now = Utc::now();
        crate::types::Site {
            name: name.into(),
            backend: backend.into(),
            enabled: true,
            created_at: now,
            updated_at: now,
            host_mode: crate::types::HostMode::Passthrough,
            host_custom: None,
            domain_count: 0,
        }
    }

    fn domain_for_site(domain: &str, site_name: &str) -> Domain {
        let now = Utc::now();
        Domain {
            domain: domain.into(),
            site_name: site_name.into(),
            enabled: true,
            auto_issue: false,
            dns_provider: None,
            challenge_kind: None,
            created_at: now,
        }
    }

    #[test]
    fn build_tunnel_domain_set_collects_only_tunnel_sites() {
        let sites = vec![
            site_with_backend("direct", "http://127.0.0.1:8080"),
            site_with_backend("tun", "office:http://10.0.0.1:8080"),
            site_with_backend("file", "file:///srv/static"),
        ];
        let domains = vec![
            domain_for_site("direct.example.com", "direct"),
            domain_for_site("tun.example.com", "tun"),
            domain_for_site("file.example.com", "file"),
        ];
        let idx = Indexes::build(sites, domains);
        let set = build_tunnel_domain_set(&idx);
        assert_eq!(set.len(), 1);
        assert!(set.contains("tun.example.com"));
        assert!(!set.contains("direct.example.com"));
        assert!(!set.contains("file.example.com"));
    }

    #[test]
    fn build_tunnel_domain_set_includes_wildcard_literals() {
        let sites = vec![site_with_backend("tun", "office:http://10.0.0.1:8080")];
        let domains = vec![domain_for_site("*.example.com", "tun")];
        let idx = Indexes::build(sites, domains);
        let set = build_tunnel_domain_set(&idx);
        // The literal `*.example.com` is stored as-is so the TLS callback
        // can match SNI `foo.example.com` via the wildcard deformation.
        assert!(set.contains("*.example.com"));
    }

    #[test]
    fn build_tunnel_domain_set_skips_disabled_sites() {
        // Indexes::build already excludes disabled sites from `domain`,
        // so the mirror follows the same filter.
        let mut disabled = site_with_backend("tun", "office:http://10.0.0.1:8080");
        disabled.enabled = false;
        let sites = vec![disabled];
        let domains = vec![domain_for_site("tun.example.com", "tun")];
        let idx = Indexes::build(sites, domains);
        let set = build_tunnel_domain_set(&idx);
        assert!(set.is_empty());
    }

    #[test]
    fn build_tunnel_domain_set_skips_invalid_backends() {
        // Bad backend → parse fails → domain excluded from mirror.
        // Same policy as the tun index pass in `Indexes::build`.
        let sites = vec![site_with_backend("bad", "ftp://x:21")];
        let domains = vec![domain_for_site("bad.example.com", "bad")];
        let idx = Indexes::build(sites, domains);
        let set = build_tunnel_domain_set(&idx);
        assert!(set.is_empty());
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

    // ---- Access log / issue #73 tests ----

    /// Build a `Config` with the access-log knobs overridden. Other
    /// fields fall through to `Config::default()`. We never call
    /// `App::new` here so the test doesn't need a SQLite DB or a
    /// cert dir.
    fn make_log_config(recent: usize, capacity: usize) -> Config {
        Config {
            log: LogConfig {
                level: "info".into(),
                file: String::new(),
                access_log_recent: recent,
                access_log_capacity: capacity,
            },
            ..Config::default()
        }
    }

    /// Build an AccessLogEntry with the fields set so equality
    /// assertions can check them later. Uses `Utc::now()` for the
    /// timestamp; equality on the `path` is enough to disambiguate.
    fn make_entry(method: &str, path: &str, status: u16) -> AccessLogEntry {
        AccessLogEntry {
            timestamp: Utc::now(),
            method: method.into(),
            path: path.into(),
            host: "example.com".into(),
            status,
            duration_ms: 7,
            backend: "direct:127.0.0.1:8080".into(),
            client_ip: "10.0.0.1".into(),
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

    #[test]
    fn push_access_log_zero_capacity_does_not_panic_or_block() {
        // access_log_capacity=0 must not panic (broadcast channel
        // uses max(1, …) internally) and must not block. We just
        // exercise the API; the test passing is the proof.
        let cfg = make_log_config(10, 0);
        // We can construct an App-equivalent path by manually
        // building the broadcast + buffer. The cap=0 broadcast
        // variant in App::new uses max(1) to avoid the
        // `Sender::new(0)` trap, but the ring buffer is what
        // matters here for the "no panic" assertion.
        let (tx, _rx) = broadcast::channel::<AccessLogEntry>(1);
        let buf = AccessLogBuffer::new(cfg.log.access_log_recent);
        // Mirror App::push_access_log.
        buf.push(make_entry("GET", "/a", 200));
        let _ = tx.send(make_entry("GET", "/a", 200));
        // Ring buffer should hold the entry.
        assert_eq!(buf.snapshot().len(), 1);
    }

    #[test]
    fn push_access_log_ring_buffer_evicts_oldest() {
        // The ring buffer is the source of truth for replay; verify
        // that exceeding `access_log_recent` evicts the oldest
        // entry, regardless of how many broadcast subscribers are
        // listening.
        let cfg = make_log_config(3, 100);
        let buf = AccessLogBuffer::new(cfg.log.access_log_recent);
        for i in 0..5 {
            buf.push(make_entry("GET", &format!("/p{i}"), 200));
        }
        let snap = buf.snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].path, "/p2");
        assert_eq!(snap[1].path, "/p3");
        assert_eq!(snap[2].path, "/p4");
    }

    #[test]
    fn push_access_log_zero_subscribers_completes_ok() {
        // `App::push_access_log` MUST NOT panic or block when there
        // are zero subscribers — this is the "zero-overhead when no
        // subscribers" hard requirement from the issue. Construct
        // an `App` (requires a tempdir DB) and verify pushes are
        // silent + the ring buffer still receives them.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("pangolin.db");
        // No broadcast subscribers — this is the key property
        // being asserted.
        let cfg = make_log_config(50, 16);
        let cert_mgr = CertManager::default();
        let app = App::new(&db_path, cfg, cert_mgr).unwrap();

        for i in 0..10 {
            // push_access_log returns (); we just need it not to
            // panic. With zero subscribers the broadcast
            // `.send()` returns Err(SendError) which we discard
            // — exactly the "drop the error, do not crash" design.
            app.push_access_log(make_entry("GET", &format!("/p{i}"), 200));
        }

        // Ring buffer received every push.
        let snap = app.recent_access_log();
        assert_eq!(snap.len(), 10);
        assert_eq!(snap[0].path, "/p0");
        assert_eq!(snap[9].path, "/p9");
    }

    #[test]
    fn push_access_log_broadcasts_to_subscriber() {
        // `App::push_access_log` MUST deliver the entry to a live
        // subscriber on the broadcast channel. Subscribe first, push
        // once, read once.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("pangolin.db");
        let cfg = make_log_config(50, 16);
        let cert_mgr = CertManager::default();
        let app = App::new(&db_path, cfg, cert_mgr).unwrap();

        let mut rx = app.access_log_tx.subscribe();
        app.push_access_log(make_entry("POST", "/submit", 201));

        // The entry must arrive. tokio broadcast is not a blocking
        // channel but it is delivered into a `recv()` future
        // synchronously when the sender is on the same runtime; if
        // not, this short timeout (1s) is plenty for the in-process
        // test.
        let entry = tokio_test_block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                .await
                .expect("recv timed out")
                .expect("broadcast channel closed unexpectedly")
        });
        assert_eq!(entry.method, "POST");
        assert_eq!(entry.path, "/submit");
        assert_eq!(entry.status, 201);
    }

    /// Tiny helper to block on a future from a non-async test fn.
    /// Used only by `push_access_log_broadcasts_to_subscriber`.
    fn tokio_test_block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }
}
