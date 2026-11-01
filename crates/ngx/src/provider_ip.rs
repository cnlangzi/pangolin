//! Background fetch of the providers' published source-IP ranges
//! (issue #122).
//!
//! The origin guard needs to know which addresses the front CDN
//! connects from. The provider registry says *where* that list lives;
//! this module keeps it fresh:
//!
//! * on start, the persisted lists are loaded into memory, so the
//!   guard works before the first fetch completes;
//! * every interval, each URL of every provider is fetched with a
//!   **conditional request** (`If-None-Match` / `If-Modified-Since`);
//! * a `304 Not Modified` keeps the stored ranges;
//! * a `200` replaces the stored ranges, the validators and the
//!   timestamp;
//! * a failure (transport error, HTTP error, malformed body) keeps
//!   the stored ranges in force and records the error — a network
//!   blip must not take a production site down;
//! * the retry interval backs off exponentially and resets after a
//!   success.
//!
//! The gateway process is the only one that fetches: `reqwest` is a
//! dependency of the `ngx` crate, not of `pangolin-core`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use log::{debug, error, info, warn};
use pangolin_core::cidr::CidrSet;
use pangolin_core::provider::{ProviderIpList, ProviderKey, RangeParser, parse_ranges, spec};

use crate::App;
use crate::runtime::{Service, ServiceContext};

/// The delay before the first retry after a failure.
pub const BASE_BACKOFF: Duration = Duration::from_secs(60);

/// The longest delay between two retries after a failure.
pub const MAX_BACKOFF: Duration = Duration::from_secs(1800);

/// The timeout of one HTTP request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The outcome of one range-list fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchOutcome {
    /// The list was downloaded and applied. The value is the number
    /// of ranges it holds.
    Updated { ranges: usize },
    /// The server answered `304 Not Modified` — the stored ranges
    /// stay in force.
    NotModified,
}

/// The service that keeps the providers' published source-IP ranges
/// up to date.
///
/// Registered in `ngx::main` next to `acme::AcmeService` and
/// `tunnel::TunnelService`.
pub struct ProviderIpService {
    /// The HTTP client used to fetch the lists.
    client: reqwest::Client,
    /// The interval between two refreshes.
    interval: Duration,
    /// The delay before the first retry after a failure.
    base_backoff: Duration,
    /// The longest delay between two retries after a failure.
    max_backoff: Duration,
}

impl ProviderIpService {
    /// Build the service with the configured refresh interval.
    pub fn new(interval: Duration) -> anyhow::Result<Self> {
        Ok(Self {
            client: build_http_client()?,
            interval,
            base_backoff: BASE_BACKOFF,
            max_backoff: MAX_BACKOFF,
        })
    }

    /// Load the persisted lists into memory.
    ///
    /// The guard has to work before the first fetch completes (and on
    /// a restart, when the network may be down), so the stored ranges
    /// are the starting point.
    pub async fn load_persisted(&self, app: &Arc<App>) {
        let stored = {
            let db = app.db.lock().await;
            pangolin_core::db::list_provider_ip_lists(&db).unwrap_or_default()
        };
        let mut sets: std::collections::HashMap<ProviderKey, CidrSet> =
            std::collections::HashMap::new();
        for row in &stored {
            let set = sets.entry(row.provider).or_default();
            for cidr in row.cidrs.lines() {
                let cidr = cidr.trim();
                if cidr.is_empty() {
                    continue;
                }
                let _ = set.insert(cidr);
            }
        }
        let mut ranges = app.provider_ranges.write();
        for (key, set) in sets {
            info!("provider ranges: loaded {} range(s) for {}", set.len(), key);
            ranges.insert(key, Arc::new(set));
        }
    }

    /// Fetch every URL of every provider once.
    ///
    /// Returns `true` when every fetch succeeded.
    pub async fn refresh_once(&self, app: &Arc<App>) -> bool {
        let mut all_ok = true;
        for key in ProviderKey::all() {
            let Some(source) = spec(*key).ranges else {
                continue;
            };
            for url in source.urls {
                match fetch_range_list(app, &self.client, *key, url.url).await {
                    Ok(FetchOutcome::Updated { ranges }) => {
                        info!(
                            "provider ranges: {} / {} — {} range(s)",
                            key, url.url, ranges
                        );
                    }
                    Ok(FetchOutcome::NotModified) => {
                        debug!("provider ranges: {} / {} — not modified", key, url.url);
                    }
                    Err(e) => {
                        // The stored ranges stay in force; the error is
                        // recorded and shown in the admin UI.
                        all_ok = false;
                        warn!("provider ranges: {} / {} — {e}", key, url.url);
                    }
                }
            }
        }
        all_ok
    }
}

#[async_trait]
impl Service for ProviderIpService {
    fn name(&self) -> &'static str {
        "provider-ip"
    }

    async fn run(&self, ctx: ServiceContext) -> anyhow::Result<()> {
        // The persisted lists first: the guard must work before the
        // first fetch completes.
        self.load_persisted(&ctx.app).await;

        let mut backoff = self.base_backoff;
        loop {
            let ok = self.refresh_once(&ctx.app).await;
            let delay = if ok {
                backoff = self.base_backoff;
                self.interval
            } else {
                let d = backoff;
                backoff = (backoff * 2).min(self.max_backoff);
                d
            };

            tokio::select! {
                _ = ctx.shutdown.cancelled() => {
                    info!("provider ranges: shutting down");
                    break;
                }
                _ = tokio::time::sleep(delay) => {}
            }
        }
        Ok(())
    }
}

/// Fetch the range list of `provider` at `url` and apply it.
///
/// The stored ranges are **never** removed by a failed fetch: the
/// error is recorded (status + error text) and the stored list stays
/// in force.
///
/// `url` is the registry's URL in production; the tests point it at a
/// stub server.
pub async fn fetch_range_list(
    app: &Arc<App>,
    client: &reqwest::Client,
    provider: ProviderKey,
    url: &str,
) -> Result<FetchOutcome, String> {
    match fetch_range_list_inner(app, client, provider, url).await {
        Ok(outcome) => Ok(outcome),
        Err(e) => {
            record_error(app, provider, url, &e).await;
            Err(e)
        }
    }
}

/// The request + parse half of [`fetch_range_list`].
async fn fetch_range_list_inner(
    app: &Arc<App>,
    client: &reqwest::Client,
    provider: ProviderKey,
    url: &str,
) -> Result<FetchOutcome, String> {
    let stored = {
        let db = app.db.lock().await;
        pangolin_core::db::list_provider_ip_lists(&db)
            .unwrap_or_default()
            .into_iter()
            .find(|r| r.provider == provider && r.url == url)
    };

    let mut req = client.get(url);
    if let Some(etag) = stored.as_ref().and_then(|r| r.etag.as_ref()) {
        req = req.header(reqwest::header::IF_NONE_MATCH, etag);
    }
    if let Some(lm) = stored.as_ref().and_then(|r| r.last_modified.as_ref()) {
        req = req.header(reqwest::header::IF_MODIFIED_SINCE, lm);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    let etag = header_value(&resp, reqwest::header::ETAG);
    let last_modified = header_value(&resp, reqwest::header::LAST_MODIFIED);

    if status == reqwest::StatusCode::NOT_MODIFIED {
        // The stored ranges stay; only the timestamp moves.
        let ranges = stored.as_ref().map(|r| r.cidrs.clone()).unwrap_or_default();
        let etag = etag.or_else(|| stored.as_ref().and_then(|r| r.etag.clone()));
        let last_modified =
            last_modified.or_else(|| stored.as_ref().and_then(|r| r.last_modified.clone()));
        write_list(
            app,
            ProviderIpList {
                provider,
                url: url.to_string(),
                cidrs: ranges,
                fetched_at: chrono::Utc::now(),
                etag,
                last_modified,
                status: pangolin_core::provider::STATUS_OK.to_string(),
                last_error: None,
            },
        )
        .await
        .map_err(|e| format!("storing the range list failed: {e}"))?;
        rebuild_provider_ranges(app, provider).await;
        return Ok(FetchOutcome::NotModified);
    }

    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| format!("reading the body failed: {e}"))?;

    // A malformed line fails the whole fetch: a truncated or
    // substituted body must not silently shrink the allowlist.
    let parser = spec(provider)
        .ranges
        .map(|r| r.parser)
        .unwrap_or(RangeParser::TextLines);
    let set = parse_ranges(parser, &body)?;

    let cidrs = set.to_cidrs().join("\n");
    write_list(
        app,
        ProviderIpList {
            provider,
            url: url.to_string(),
            cidrs,
            fetched_at: chrono::Utc::now(),
            etag,
            last_modified,
            status: pangolin_core::provider::STATUS_OK.to_string(),
            last_error: None,
        },
    )
    .await
    .map_err(|e| format!("storing the range list failed: {e}"))?;
    rebuild_provider_ranges(app, provider).await;
    Ok(FetchOutcome::Updated { ranges: set.len() })
}

/// Record a failed fetch: the stored ranges stay in force, the status
/// becomes `error` and the error text is kept for the admin UI.
async fn record_error(app: &Arc<App>, provider: ProviderKey, url: &str, message: &str) {
    let stored = {
        let db = app.db.lock().await;
        pangolin_core::db::list_provider_ip_lists(&db)
            .unwrap_or_default()
            .into_iter()
            .find(|r| r.provider == provider && r.url == url)
    };
    let row = ProviderIpList {
        provider,
        url: url.to_string(),
        cidrs: stored.as_ref().map(|r| r.cidrs.clone()).unwrap_or_default(),
        fetched_at: chrono::Utc::now(),
        etag: stored.as_ref().and_then(|r| r.etag.clone()),
        last_modified: stored.as_ref().and_then(|r| r.last_modified.clone()),
        status: pangolin_core::provider::STATUS_ERROR.to_string(),
        last_error: Some(message.to_string()),
    };
    if let Err(e) = write_list(app, row).await {
        error!("provider ranges: recording the failure of {provider} / {url} failed: {e}");
    }
}

/// Persist one range list.
async fn write_list(app: &Arc<App>, row: ProviderIpList) -> Result<(), String> {
    let db = app.db.lock().await;
    pangolin_core::db::upsert_provider_ip_list(&db, &row)
        .map_err(|e| format!("storing the range list failed: {e}"))
}

/// Recompute the in-memory range set of a provider from the stored
/// lists.
///
/// The set is the union of the lists of all the provider's URLs, so
/// one URL changing does not drop the others.
pub async fn rebuild_provider_ranges(app: &Arc<App>, provider: ProviderKey) {
    let stored = {
        let db = app.db.lock().await;
        pangolin_core::db::list_provider_ip_lists(&db).unwrap_or_default()
    };
    let mut set = CidrSet::new();
    for row in stored.iter().filter(|r| r.provider == provider) {
        for cidr in row.cidrs.lines() {
            let cidr = cidr.trim();
            if cidr.is_empty() {
                continue;
            }
            let _ = set.insert(cidr);
        }
    }
    let len = set.len();
    app.provider_ranges.write().insert(provider, Arc::new(set));
    debug!("provider ranges: {} now holds {} range(s)", provider, len);
}

/// Read one header of a response as a string.
fn header_value(resp: &reqwest::Response, name: reqwest::header::HeaderName) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_string())
}

/// Build the HTTP client used to fetch the range lists.
///
/// The requests are **verified**: the range list decides which
/// addresses may reach the origin, so an attacker who could tamper
/// with it could widen the allowlist. The root store is the system's
/// own (the same one the ACME client relies on to talk to the CA).
fn build_http_client() -> anyhow::Result<reqwest::Client> {
    let certs = rustls_native_certs::load_native_certs()
        .map_err(|e| anyhow::anyhow!("loading the native certificate store failed: {e}"))?;
    if certs.is_empty() {
        warn!(
            "provider ranges: the native certificate store is empty — the range lists \
             cannot be fetched over HTTPS"
        );
    }
    let mut builder = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("pangolin-ngx/", env!("CARGO_PKG_VERSION")));
    for cert in certs {
        // A certificate reqwest rejects is skipped — the others
        // still make the client usable.
        match reqwest::Certificate::from_der(cert.as_ref()) {
            Ok(c) => builder = builder.add_root_certificate(c),
            Err(e) => warn!("provider ranges: skipping a native certificate: {e}"),
        }
    }
    Ok(builder.build()?)
}
