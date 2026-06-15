//! Pangolin core types. Mirrors the SQL schema in README.md.
//!
//! All primary keys are natural TEXT keys (no surrogate `id INTEGER`).
//! This matches the README's "全部 TEXT 主键" decision and removes
//! the need for ID/FK indirection.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// How the Host header is set when proxying to the backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum HostMode {
    /// Use the backend URL's host (IP or domain) as-is.
    Backend,
    /// Pass through the original Host header from the client.
    #[default]
    Passthrough,
    /// Use a custom host value, and add X-Forwarded-Host with the original.
    Custom,
}

impl std::fmt::Display for HostMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostMode::Backend => write!(f, "backend"),
            HostMode::Passthrough => write!(f, "passthrough"),
            HostMode::Custom => write!(f, "custom"),
        }
    }
}

impl std::str::FromStr for HostMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "backend" => Ok(HostMode::Backend),
            "passthrough" => Ok(HostMode::Passthrough),
            "custom" => Ok(HostMode::Custom),
            _ => Err(format!("unknown host_mode: {}", s)),
        }
    }
}

/// Site (sites table). name is the primary key.
/// domain_count is a denormalised count populated at list-time for UI convenience.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Site {
    pub name: String,
    pub backend: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// How to set the Host header when proxying to backend.
    #[serde(default)]
    pub host_mode: HostMode,
    /// Custom host value (used when host_mode is Custom).
    #[serde(default)]
    pub host_custom: Option<String>,
    /// Denormalised domain count for the sites table UI. Not stored in DB.
    #[serde(default)]
    pub domain_count: usize,
}

impl Site {
    /// Returns true if host_mode is Passthrough (default).
    pub fn is_host_mode_passthrough(&self) -> bool {
        self.host_mode == HostMode::Passthrough
    }
    /// Returns true if host_mode is Backend.
    pub fn is_host_mode_backend(&self) -> bool {
        self.host_mode == HostMode::Backend
    }
    /// Returns true if host_mode is Custom.
    pub fn is_host_mode_custom(&self) -> bool {
        self.host_mode == HostMode::Custom
    }

    /// Parses the `backend` field into its three display components for the
    /// hierarchical site form:
    ///   - `route_mode`: "direct" if no `tun:` prefix, "tunnel" otherwise.
    ///   - `tun_name`: the tunnel name prefix (empty for direct).
    ///   - `scheme`: the URL scheme ("http", "https", "file", or "" if
    ///     unparseable — we keep the field empty rather than erroring at
    ///     render time so a malformed stored value still loads the page).
    ///   - `host_port`: the part after `scheme://` (e.g. `127.0.0.1:8080`).
    ///
    /// See `parse::parse_backend` for the canonical parser. These helpers
    /// exist for templates that want to pre-fill the three-step UI without
    /// round-tripping through `parse_backend`'s `Result`.
    pub fn backend_route_mode(&self) -> &'static str {
        if self.backend.is_empty() {
            return "direct";
        }
        if self.backend.starts_with("http://")
            || self.backend.starts_with("https://")
            || self.backend.starts_with("file:///")
        {
            "direct"
        } else {
            // Anything else with a colon is assumed to be `tun_name:scheme://...`.
            // We don't try to validate here; the form submit will be rejected
            // server-side if malformed.
            "tunnel"
        }
    }

    /// Tunnel name prefix. Empty string for direct mode.
    pub fn backend_tun_name(&self) -> &str {
        if self.backend_route_mode() != "tunnel" {
            return "";
        }
        match self.backend.find(':') {
            Some(idx) => &self.backend[..idx],
            None => "",
        }
    }

    /// URL scheme (lowercased): "http", "https", "file", or "".
    /// For tunnel backends (`tun:scheme://...`), returns the scheme of the
    /// URL portion, not the tunnel name.
    pub fn backend_scheme(&self) -> &str {
        if self.backend_route_mode() == "tunnel"
            && let Some(colon_idx) = self.backend.find(':')
        {
            return detect_scheme(&self.backend[colon_idx + 1..]);
        }
        detect_scheme(&self.backend)
    }

    /// Host (and optional port) portion of the URL, i.e. the part after
    /// `scheme://` and before any path. For `file:///var/www` this returns
    /// `var/www` (no host), which is what the user types into the
    /// hierarchical form's host:port field.
    ///
    /// For tunnel-mode URLs (`tun:scheme://...`) we first strip the
    /// `tun_name:` prefix and then re-apply scheme stripping. This is
    /// important: applying `split_host_path` directly to `http://host:port`
    /// would split at the first `/` (right after `http:`) and return the
    /// bogus string `http:`. The form's JS round-trip relies on getting
    /// just the host:port back.
    pub fn backend_host_port(&self) -> &str {
        if self.backend_route_mode() == "tunnel"
            && let Some(colon_idx) = self.backend.find(':')
        {
            return strip_scheme_and_split(&self.backend[colon_idx + 1..]);
        }
        strip_scheme_and_split(&self.backend)
    }
}

/// Returns the URL scheme of a backend string, or "" if none of the
/// known schemes match. Used by `Site::backend_scheme` to detect the
/// `http`/`https`/`file` prefix on either direct or tunnel-mode URLs.
fn detect_scheme(url: &str) -> &'static str {
    if url.starts_with("https://") {
        "https"
    } else if url.starts_with("http://") {
        "http"
    } else if url.starts_with("file:///") {
        "file"
    } else {
        ""
    }
}

/// Strips the URL scheme prefix and splits host from path. For
/// `file:///var/www` this returns `/var/www` (with the leading slash
/// preserved, so the round-trip through the form's host:port field
/// keeps the full path). Used by `Site::backend_host_port` to operate
/// on `host[:port][/path]` rather than the full `scheme://host:port[/path]`
/// URL.
fn strip_scheme_and_split(url: &str) -> &str {
    if let Some(rest) = url.strip_prefix("https://") {
        return split_host_path(rest);
    }
    if let Some(rest) = url.strip_prefix("http://") {
        return split_host_path(rest);
    }
    if let Some(rest) = url.strip_prefix("file://") {
        return rest;
    }
    ""
}

/// Splits an `host[:port][/path]` string at the first `/`, returning the
/// `host[:port]` portion. Used by `Site::backend_host_port` to strip paths
/// from URLs like `https://example.com/api` → `example.com`.
fn split_host_path(s: &str) -> &str {
    match s.find('/') {
        Some(idx) => &s[..idx],
        None => s,
    }
}

/// Domain (domains table). domain is the primary key.
/// site_name references sites.name (logical FK; not enforced at SQL level
/// because we want fast reload without per-row FK checks).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Domain {
    pub domain: String,
    pub site_name: String,
    pub enabled: bool,
    /// If true, this domain is managed by ACME auto-issuance.
    /// If false (default), the operator is expected to manage certs manually
    /// (or the domain is HTTP-only). Wildcard domains must have this set to true.
    #[serde(default)]
    pub auto_issue: bool,
    /// Name of the dns_providers row used to validate this domain (FQDN or base).
    /// None = no DNS-01 association; ACME will fall back to HTTP-01 (wildcards fail).
    #[serde(default)]
    pub dns_provider: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Tun node (tun table). name is the primary key.
///
/// v2: `token` is now a column on `tun` itself. Auth model is
/// "the WS query presents (name, token); a single SELECT confirms
/// both match an enabled, non-expired row." Auto-register on first
/// sight is the default for any new (name, token) pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tun {
    pub name: String,
    /// The auth credential this tun presents in the WS query string.
    /// `None` until the tun has been seen at least once or an admin
    /// has provisioned the row via the admin API.
    ///
    /// V3: this is now the legacy cleartext column. The on-disk
    /// source of truth is `token_hash` (sha256 hex); this field
    /// is kept populated for UI display + downgrade safety.
    pub token: Option<String>,
    /// sha256(token) hex (lowercase, 64 chars). Populated by
    /// `db::upsert_tun` whenever a token is set. Compared by
    /// `db::auth_tun` against the sha256 of the inbound bearer.
    #[serde(default)]
    pub token_hash: Option<String>,
    pub enabled: bool,
    pub online: bool,
    pub registered_at: Option<DateTime<Utc>>,
    pub last_seen_at: Option<DateTime<Utc>>,
    /// Per-tun token expiry. `None` = never expires.
    pub expires_at: Option<DateTime<Utc>>,
}

/// Lifecycle state of a `certs` row (issue #45).
///
/// Until v3 the `certs` table only carried fully-issued rows. With auto-issue
/// becoming the normal path, the table has to surface in-flight and failed
/// rows too — otherwise the dashboard reports a false sense of completeness
/// and operators cannot retry without SSH/restart. The five-state machine
/// below is the minimum that covers the full lifecycle without ambiguity:
///
/// ```text
///   upsert_domain(auto_issue=true) ──► Pending
///                                          │ ensure_one() starts ACME
///                                          ▼
///                                       Issuing
///                              success  ╱        ╲  failure
///                                      ▼          ▼
///                                   Issued     Failed
///   plan_issuance() returns "no plan possible" ──► Skipped
///   Manual upload via /certs/new ─────────────► Issued (direct)
/// ```
///
/// All variants serialize as lowercase to match the on-disk DB representation
/// (`certs.status`). The V5-added `RateLimited` uses snake_case
/// (`rate_limited`) to keep the DB row legible when scanned by hand;
/// the `snake_case` rename rule keeps serde's output in lockstep
/// with [`CertStatus::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CertStatus {
    /// Auto-issue requested but ACME hasn't started yet. Set by
    /// `upsert_domain` when `auto_issue=true` so the operator immediately
    /// sees the row on the dashboard.
    Pending,
    /// ACME flow is currently running (DNS-01 / HTTP-01 challenge,
    /// CSR submission, polling for the issued cert).
    Issuing,
    /// Cert is on disk and valid. Default for backward compatibility:
    /// pre-V4 manual rows migrate cleanly to this state.
    Issued,
    /// Last ACME attempt errored. `last_error` carries the failure
    /// message; an operator can retry via `POST /certs/retry`.
    Failed,
    /// `plan_issuance` decided the row cannot be issued (e.g. wildcard
    /// without DNS association). `last_error` carries the reason. Distinct
    /// from `Failed` because retrying without fixing config will not help.
    Skipped,
    /// The ACME server told us we're being rate-limited (HTTP 429 or
    /// `urn:ietf:params:acme:error:rateLimited`). The dashboard
    /// renders this as a distinct color (purple vs red) and shows the
    /// server-supplied retry timestamp from `next_retry_at`. The loop
    /// will not touch this row until `next_retry_at` has passed.
    RateLimited,
    /// The ACME server returned a permanent rejection that no amount
    /// of retrying will resolve (e.g. `rejectedIdentifier` for a
    /// domain with no dot, `invalid` for a malformed CSR, `caa`
    /// for a CAA-policy violation). The retry button is hidden; the
    /// operator must fix the underlying issue and re-trigger via
    /// `POST /certs/retry`.
    Permanent,
}

impl CertStatus {
    /// Lowercase string used in the DB (matches `serde(rename_all = "lowercase")`).
    pub fn as_str(self) -> &'static str {
        match self {
            CertStatus::Pending => "pending",
            CertStatus::Issuing => "issuing",
            CertStatus::Issued => "issued",
            CertStatus::Failed => "failed",
            CertStatus::Skipped => "skipped",
            CertStatus::RateLimited => "rate_limited",
            CertStatus::Permanent => "permanent",
        }
    }

    /// Default for pre-V4 rows (in the DB via `DEFAULT 'issued'`).
    pub fn default_for_legacy() -> Self {
        CertStatus::Issued
    }

    /// True when an operator-facing retry button makes sense. The UI uses
    /// this to decide whether to render the ↻ icon next to the row.
    /// `Permanent` is intentionally excluded — the operator must fix
    /// the underlying issue (domain, DNS, CA policy) first, so
    /// "retry" would just hammer the server with the same rejected
    /// request. `RateLimited` is included because the server has
    /// told us when to come back.
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            CertStatus::Failed
                | CertStatus::Pending
                | CertStatus::Skipped
                | CertStatus::RateLimited
                | CertStatus::Permanent
        )
    }

    /// True when the renewal loop should not touch this row at all,
    /// even if `next_retry_at` has passed. Currently only `Permanent`
    /// — the loop requires a manual `POST /certs/retry` to clear it.
    pub fn is_terminal(self) -> bool {
        matches!(self, CertStatus::Permanent)
    }

    /// Every status variant in DB-storage order. Used by `count_certs_by_status`
    /// to ensure every key is present in the result map (zero-valued when no
    /// rows match) — the dashboard summary endpoint relies on this.
    pub fn all() -> [CertStatus; 7] {
        [
            CertStatus::Pending,
            CertStatus::Issuing,
            CertStatus::Issued,
            CertStatus::Failed,
            CertStatus::Skipped,
            CertStatus::RateLimited,
            CertStatus::Permanent,
        ]
    }
}

impl std::fmt::Display for CertStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for CertStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(CertStatus::Pending),
            "issuing" => Ok(CertStatus::Issuing),
            "issued" => Ok(CertStatus::Issued),
            "failed" => Ok(CertStatus::Failed),
            "skipped" => Ok(CertStatus::Skipped),
            "rate_limited" => Ok(CertStatus::RateLimited),
            "permanent" => Ok(CertStatus::Permanent),
            other => Err(format!("unknown cert status: {other}")),
        }
    }
}

/// Classification of an ACME attempt failure. Drives whether the next
/// scan will retry, and how long it will wait. Modelled on
/// [Caddy CertMagic's `ErrNoRetry` wrapper][cm] and
/// [acme4j's `AcmeServerException.getRetryAfter()`][acme4j].
///
/// [cm]: https://github.com/caddyserver/certmagic/blob/master/acmeissuer.go
/// [acme4j]: https://github.com/shred/acme4j/blob/master/acme4j-client/src/main/java/org/shredzone/acme4j/exception/AcmeServerException.java
///
/// Serializes into the `certs.error_class` column as a compact string:
///   "transient"            → CertErrorClass::Transient
///   "permanent"            → CertErrorClass::Permanent
///   "rate_limited:<rfc3339>" → CertErrorClass::RateLimited { retry_at }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum CertErrorClass {
    /// Network timeout, 5xx, DNS lookup failure. The renewal loop
    /// applies a backoff schedule (1m → 30m → 1h → 6h) and retries
    /// automatically.
    Transient,
    /// ACME server returned a rejection that no amount of retrying
    /// will resolve: `rejectedIdentifier` (e.g. domain with no dot),
    /// `invalid` (malformed CSR), `unauthorized` (account not
    /// authorized for the identifier), `caa` (CAA record blocks us),
    /// `dns` (DNS-01 record not set up), `connection` (HTTP-01
    /// challenge couldn't reach port 80). The renewal loop will
    /// **not** retry this row; an operator must fix the underlying
    /// issue and `POST /certs/retry`.
    Permanent,
    /// The server told us we're being rate-limited (HTTP 429 or
    /// `urn:ietf:params:acme:error:rateLimited`). `retry_at` is the
    /// server's hint (parsed from the `Retry-After` header when
    /// instant-acme surfaces it, or from the ACME problem detail
    /// string "retry after 2026-06-15 19:28:55 UTC"). The renewal
    /// loop will not touch this row until `retry_at` has passed.
    RateLimited { retry_at: DateTime<Utc> },
}

impl CertErrorClass {
    /// Compact DB representation. `RateLimited` carries its retry
    /// timestamp in the same string so the column is a single TEXT
    /// cell — no separate `next_retry_at` is needed for the
    /// rate-limited case, but we still write `next_retry_at` so the
    /// renewal loop's index can find due rows cheaply.
    pub fn as_str(&self) -> String {
        match self {
            CertErrorClass::Transient => "transient".to_string(),
            CertErrorClass::Permanent => "permanent".to_string(),
            CertErrorClass::RateLimited { retry_at } => {
                format!("rate_limited:{}", retry_at.to_rfc3339())
            }
        }
    }

    /// Parse the DB representation back into an enum. Returns `None`
    /// on unrecognized strings (forward-compatible — future classes
    /// will simply round-trip as `None` and surface as Transient).
    pub fn parse(s: &str) -> Option<Self> {
        if s == "transient" {
            Some(CertErrorClass::Transient)
        } else if s == "permanent" {
            Some(CertErrorClass::Permanent)
        } else if let Some(rest) = s.strip_prefix("rate_limited:") {
            chrono::DateTime::parse_from_rfc3339(rest)
                .ok()
                .map(|dt| CertErrorClass::RateLimited {
                    retry_at: dt.with_timezone(&Utc),
                })
        } else {
            None
        }
    }
}

/// The CertMagic-style backoff schedule. Each entry is the wait
/// before attempt N+1, given that attempt N just failed. After
/// 30 entries (each capped at 6h), the schedule bottoms out at
/// 6h — the loop will keep trying every 6h until `next_retry_at`
/// is no longer in the future or an operator intervenes.
///
/// Total budget from the schedule is roughly:
///   1 + 1 + 2 + 5 + 10 + 30 + 60 + 120 + 240 + (21 × 360) = 7940 min ≈ 5.5 days.
/// Within the 14-day `renew_threshold_days` budget, this gives
/// multiple chances to recover from a transient outage before the
/// cert actually expires.
pub const ACME_BACKOFF_SCHEDULE_SECS: &[u64] = &[
    60, 60, 120, 300, 600, 1800, 3600, 7200, 14400,
    21600, // 1m, 1m, 2m, 5m, 10m, 30m, 1h, 2h, 4h, 6h
    21600, 21600, 21600, 21600, 21600, 21600, 21600, 21600, // 8 × 6h
    21600, 21600, 21600, 21600, 21600, 21600, 21600, 21600, // 8 × 6h
    21600, 21600, 21600, 21600, 21600, 21600, 21600, 21600, // 8 × 6h
];

/// Pick the wait duration for the next retry of an attempt that
/// just failed. `attempt_count` is the number of attempts that have
/// already happened for this row in the current failure streak.
pub fn next_backoff(attempt_count: u32) -> std::time::Duration {
    let idx = (attempt_count as usize).min(ACME_BACKOFF_SCHEDULE_SECS.len() - 1);
    std::time::Duration::from_secs(ACME_BACKOFF_SCHEDULE_SECS[idx])
}

/// Certificate (certs table). domain is the primary key (1:1).
/// In the new blob layout, cert_file == key_file (both point to the same blob path).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cert {
    pub domain: String,
    /// Path to the blob file (key+cert combined). Equal to key_file.
    pub cert_file: String,
    /// Path to the blob file (key+cert combined). Equal to cert_file.
    pub key_file: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    /// SAN list as JSON array string, e.g. `["example.com","www.example.com"]`.
    #[serde(default)]
    pub sans: Vec<String>,
    /// Source: "acme" or "manual".
    #[serde(default = "default_cert_source")]
    pub source: String,
    /// ACME DNS provider used for issuance (cloudflare|aliyun|tencent).
    #[serde(default)]
    pub acme_dns_provider: Option<String>,
    /// ACME account identifier used for issuance.
    #[serde(default)]
    pub acme_account_id: Option<String>,
    /// When the cert was issued (Unix timestamp seconds).
    #[serde(default)]
    pub issued_at: i64,
    /// Lifecycle state (V4). Manual rows default to `Issued`; ACME rows go
    /// through `Pending`→`Issuing`→`Issued`/`Failed`/`Skipped`.
    #[serde(default = "default_cert_status")]
    pub status: CertStatus,
    /// When the most recent ACME attempt started (V4). `None` on manual
    /// uploads. Updated to `Utc::now()` on each `POST /certs/retry`.
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    /// Most recent failure / skip reason (V4). Cleared on successful
    /// transition to `Issued`.
    #[serde(default)]
    pub last_error: Option<String>,
    /// Earliest UTC timestamp at which the renewal loop should retry
    /// this row. Set on every failure (RateLimited uses the server's
    /// `Retry-After`; Transient uses the backoff schedule). Cleared
    /// on `Issued` and on `recover_stuck_issuing_rows`. NULL means
    /// "no scheduled retry" (e.g. fresh `Pending` row, or `Issued`).
    #[serde(default)]
    pub next_retry_at: Option<DateTime<Utc>>,
    /// Classified error from the last failed attempt. Drives both
    /// the UI badge (rate-limited vs permanent get distinct colors)
    /// and the renewal loop's retry policy. `None` for `Issued` rows
    /// and for rows that have never been attempted.
    #[serde(default)]
    pub error_class: Option<CertErrorClass>,
    /// Monotonic counter of failed attempts in the current failure
    /// streak. Reset to 0 on successful `Issued` transition. Used
    /// by `next_backoff` to pick the right slot in the schedule.
    #[serde(default)]
    pub attempt_count: u32,
    /// instant-acme Order URL. Set after the order is created and
    /// persisted across restarts so the renewal loop can resume
    /// polling an in-flight order instead of opening a fresh one.
    /// Cleared on `Issued`, on `recover_stuck_issuing_rows`, and on
    /// transition to a terminal status.
    #[serde(default)]
    pub order_url: Option<String>,
}

fn default_cert_source() -> String {
    "manual".to_string()
}

fn default_cert_status() -> CertStatus {
    CertStatus::default_for_legacy()
}

/// DNS provider kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DnsProviderKind {
    Cloudflare,
    Aliyun,
    Tencent,
}

impl std::str::FromStr for DnsProviderKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "cloudflare" => Ok(DnsProviderKind::Cloudflare),
            "aliyun" => Ok(DnsProviderKind::Aliyun),
            "tencent" => Ok(DnsProviderKind::Tencent),
            other => Err(format!("unknown dns provider kind: {other}")),
        }
    }
}

impl std::fmt::Display for DnsProviderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DnsProviderKind::Cloudflare => f.write_str("cloudflare"),
            DnsProviderKind::Aliyun => f.write_str("aliyun"),
            DnsProviderKind::Tencent => f.write_str("tencent"),
        }
    }
}

/// DNS provider (dns_providers table). name is the primary key.
/// `config` is a kind-specific JSON blob holding credentials in plaintext.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsProvider {
    pub name: String,
    pub kind: DnsProviderKind,
    pub enabled: bool,
    pub config: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// ACME challenge type chosen for a SAN identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChallengeType {
    /// HTTP-01: write a challenge file under `./certs/.well-known/acme-challenge/<token>`.
    Http01,
    /// DNS-01: create a `_acme-challenge.<domain>` TXT record via the
    /// associated DNS provider.
    Dns01,
}

/// Result of `parse_backend` — what kind of upstream this site is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendKind {
    /// No `name:` prefix — ngx itself proxies to a direct URL.
    /// Covers `http://`, `https://`, `file:///`.
    Direct,
    /// Has a `name:` prefix that resolves to a known online tun node.
    Tunnel { tun_name: String },
}

// ---- Tunnel messages (used by both ngx and tun) ----
//
// As of issue #39, the tunnel carries raw HTTP/1.1 bytes inside
// yamux streams (one stream per request, or per WS connection).
// There is no longer a custom frame layer, so the only
// cross-process control-plane message left is the tunnel
// registration event surfaced to the admin UI. The actual
// request/response payload is opaque bytes on a yamux stream.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_serialize_roundtrip() {
        let s = Site {
            name: "customer-web".into(),
            backend: "office:http://192.168.1.100:8080".into(),
            enabled: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            host_mode: HostMode::Passthrough,
            host_custom: None,
            domain_count: 0,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: Site = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn domain_serialize_roundtrip() {
        let d = Domain {
            domain: "app.example.com".into(),
            site_name: "customer-web".into(),
            enabled: true,
            auto_issue: false,
            dns_provider: None,
            created_at: Utc::now(),
        };
        let json = serde_json::to_string(&d).unwrap();
        let back: Domain = serde_json::from_str(&json).unwrap();
        assert_eq!(d, back);
    }

    #[test]
    fn domain_dns_provider_roundtrip() {
        let d = Domain {
            domain: "*.example.com".into(),
            site_name: "customer-web".into(),
            enabled: true,
            auto_issue: true,
            dns_provider: Some("main-cf".into()),
            created_at: Utc::now(),
        };
        let json = serde_json::to_string(&d).unwrap();
        let back: Domain = serde_json::from_str(&json).unwrap();
        assert_eq!(d, back);
        assert!(back.auto_issue);
        assert_eq!(back.dns_provider.as_deref(), Some("main-cf"));
    }

    #[test]
    fn cert_status_roundtrip() {
        // Lowercase serde repr matches what V4's DEFAULT 'issued' stores.
        for status in CertStatus::all() {
            let s = status.as_str();
            // Display, as_str, and serde all agree.
            assert_eq!(s, format!("{status}").as_str());
            assert_eq!(s.parse::<CertStatus>().unwrap(), status);
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(json, format!("\"{s}\""));
            let back: CertStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, status);
        }
        assert!("nope".parse::<CertStatus>().is_err());
    }

    #[test]
    fn cert_status_is_retryable() {
        assert!(CertStatus::Pending.is_retryable());
        assert!(CertStatus::Failed.is_retryable());
        assert!(CertStatus::Skipped.is_retryable());
        // Issued + Issuing don't get a retry button — issuing is already
        // running and issued is the success state.
        assert!(!CertStatus::Issued.is_retryable());
        assert!(!CertStatus::Issuing.is_retryable());
    }

    #[test]
    fn cert_default_status_is_issued() {
        // Manual rows constructed without an explicit status (deserialized
        // from a pre-V4 JSON dump, e.g. CLI tooling) must remain visible.
        let json = r#"{
            "domain": "example.com",
            "cert_file": "/blob",
            "key_file": "/blob",
            "expires_at": null,
            "created_at": "2026-01-01T00:00:00Z"
        }"#;
        let c: Cert = serde_json::from_str(json).unwrap();
        assert_eq!(c.status, CertStatus::Issued);
        assert!(c.started_at.is_none());
        assert!(c.last_error.is_none());
    }

    #[test]
    fn dns_provider_kind_parses() {
        let k: DnsProviderKind = "cloudflare".parse().unwrap();
        assert_eq!(k, DnsProviderKind::Cloudflare);
        let k: DnsProviderKind = "aliyun".parse().unwrap();
        assert_eq!(k, DnsProviderKind::Aliyun);
        let k: DnsProviderKind = "tencent".parse().unwrap();
        assert_eq!(k, DnsProviderKind::Tencent);
        assert!("nope".parse::<DnsProviderKind>().is_err());
    }

    // ── backend_host_port helper: round-trip cases ─────────────────
    // These cover the bug we hit in issue #25 where the tunnel-mode
    // branch was applying `split_host_path` to the full `scheme://...`
    // URL, which split at the first `/` (right after the scheme) and
    // returned `"http:"` / `"file:"` instead of the actual host / path.

    fn site_with(backend: &str) -> Site {
        Site {
            name: "s".into(),
            backend: backend.into(),
            enabled: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            host_mode: HostMode::Passthrough,
            host_custom: None,
            domain_count: 0,
        }
    }

    #[test]
    fn host_port_direct_http() {
        assert_eq!(
            site_with("http://127.0.0.1:8080").backend_host_port(),
            "127.0.0.1:8080"
        );
    }

    #[test]
    fn host_port_direct_https_with_path() {
        // Direct mode already worked; path is stripped for the form.
        assert_eq!(
            site_with("https://example.com/api/v1").backend_host_port(),
            "example.com"
        );
    }

    #[test]
    fn host_port_direct_file() {
        // The leading slash is preserved so the value round-trips through
        // the form's host:port field without losing the path's root.
        assert_eq!(
            site_with("file:///var/www/static").backend_host_port(),
            "/var/www/static"
        );
    }

    #[test]
    fn host_port_tunnel_http() {
        // The main fix: tunnel + http used to return "http:" — now correct.
        assert_eq!(
            site_with("office:http://192.168.1.100:8080").backend_host_port(),
            "192.168.1.100:8080"
        );
    }

    #[test]
    fn host_port_tunnel_https_with_path() {
        // Path is stripped in tunnel mode too (matches JS round-trip).
        assert_eq!(
            site_with("home:https://10.0.0.5:443/api").backend_host_port(),
            "10.0.0.5:443"
        );
    }

    #[test]
    fn host_port_tunnel_file() {
        // The other half of the fix: tunnel + file:// used to return "file:".
        // Leading slash preserved for round-trip through the form field.
        assert_eq!(
            site_with("office:file:///home/user/docs").backend_host_port(),
            "/home/user/docs"
        );
    }

    #[test]
    fn host_port_tunnel_file_with_subpath() {
        assert_eq!(
            site_with("office:file:///var/www/static/index.html").backend_host_port(),
            "/var/www/static/index.html"
        );
    }

    #[test]
    fn host_port_empty() {
        assert_eq!(site_with("").backend_host_port(), "");
    }
}
