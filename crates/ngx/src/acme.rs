//! ACME certificate management — issue and renew TLS certificates via Let's Encrypt.
//!
//! File layout (autocert DirCache native blob format, byte-compatible with proxy mux):
//!   {cert_dir}/
//! example.com                    ← ECDSA blob (default)
//!     example.com+rsa                ← RSA blob (only when key_type: rsa)
//!     *.example.com                  ← wildcard blob (literal `*` in filename)
//!     *.example.com+rsa             ← wildcard RSA blob
//!     www.example.com ← multi-SAN copy: identical blob, separate file
//!     acme_account+key              ← instant-acme AccountCredentials JSON
//!     .well-known/acme-challenge/{token}  ← HTTP-01 challenge files (current location)
//!
//! Key rules:
//!   - ECDSA default, no suffix. RSA uses `+rsa` suffix. Never both at same time.
//!   - Multi-SAN certs: each SAN gets its own blob file with identical content.
//!   - Wildcard: order identifiers MUST include base domain: ["example.com", "*.example.com"].
//!   - Permissions: 0600 on all written files (matches proxy mux root:root).

#![allow(dead_code)]

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use instant_acme::{Account, AccountCredentials, ChallengeType, Identifier, NewOrder, OrderStatus};
use tokio::time::sleep;

use crate::dns::{wait_for_txt_propagation, DnsProvider};

/// Account credentials file — instant-acme `AccountCredentials` JSON.
///
/// Renamed from the original `acme_account+key` (autocert DirCache
/// convention) to `acme_account.json` after issue #45 follow-up:
/// the old name had no extension, which made operators mistake it
/// for a PEM file and clobber it with `cp some.pem ./acme_account+key`
/// — at which point `serde_json::from_str` choked with the famously
/// unhelpful `"invalid number at line 1 column 2"` (the JSON parser
/// sees `-` from `-----BEGIN ...` and tries to parse a number).
/// The `.json` extension makes the format obvious to tooling and
/// operators alike.
///
/// This is the ONLY filename the loader honours. Files under any other
/// name in `cert_dir` — including the historical `acme_account+key` —
/// are ignored entirely; the loader does not read them, migrate them,
/// or warn about them. Operators upgrading from a pre-rename install
/// must `mv acme_account+key acme_account.json` themselves.
const ACCOUNT_KEY_FILE: &str = "acme_account.json";

/// Load credentials from `cert_dir/acme_account.json`. Returns `None`
/// when the file does not exist (caller registers a fresh account).
/// Returns `Err` with a human-readable, file-path-tagged message when
/// the file exists but cannot be parsed — the prior code produced
/// `"invalid number at line 1 column 2"` with no file path, which hid
/// which file to fix.
///
/// No legacy filenames are honoured: the contract is "either there is
/// an `acme_account.json` and we use it, or there isn't and we register
/// fresh." Any other `acme_account*` file is invisible to the loader.
async fn load_account_credentials(cert_dir: &Path) -> Result<Option<AccountCredentials>> {
    let path = cert_dir.join(ACCOUNT_KEY_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let content = tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("read {}", path.display()))?;
    let cred = parse_account_credentials(&content, &path)?;
    Ok(Some(cred))
}

/// Parse a string as `AccountCredentials`. Produces a detailed error
/// message — including the file path, the first bytes, and a fix
/// hint — when parsing fails. This replaces the previous bare
/// `"invalid account credentials: invalid number at line 1 column 2"`
/// which gave operators no clue which file to fix.
fn parse_account_credentials(content: &str, path: &Path) -> Result<AccountCredentials> {
    serde_json::from_str::<AccountCredentials>(content).map_err(|e| {
        // First 40 chars of the file — enough to recognise common
        // mistakes (PEM, HTML error page, empty file) without
        // leaking the key.
        let head: String = content.chars().take(40).collect();
        if content.starts_with("-----BEGIN") {
            anyhow::anyhow!(
                "ACME account file {} looks like a PEM private key, not JSON \
                 (instant-acme expects a JSON serialisation of `AccountCredentials`, \
                 not a raw key). Most likely cause: an operator accidentally \
                 overwrote the file. To recover, move the file aside \
                 (`mv {} {}.bak`) and restart — a fresh ACME account will be \
                 registered. First bytes: {:?}",
                path.display(),
                path.display(),
                path.display(),
                head,
            )
        } else if content.trim().is_empty() {
            anyhow::anyhow!(
                "ACME account file {} is empty (likely truncated by a previous \
                 crash or disk-full event). Delete it and restart to register \
                 a fresh account.",
                path.display(),
            )
        } else {
            anyhow::anyhow!(
                "ACME account file {} is not valid JSON: {}. First bytes: {:?}. \
                 If this file was corrupted, move it aside and restart to \
                 register a fresh account.",
                path.display(),
                e,
                head,
            )
        }
    })
}

/// Per-stage callback used by [`AcmeClient::issue_with_plan`] to emit
/// progress events to whoever is interested — typically the in-memory
/// `EventBuffer` so the dashboard activity panel surfaces the trace
/// live. Each call is `(stage, detail)`; the implementor decides
/// where to send it (stdout log is always emitted in addition).
///
/// Boxed and Arc'd so the same callback can be cheaply shared across
/// the `await`s inside `issue_with_plan` without lifetime juggling.
pub type IssueTrace = Arc<dyn Fn(&str, String) + Send + Sync>;

/// ACME client for issuing and renewing certificates.
pub struct AcmeClient {
    account: Account,
    cert_dir: PathBuf,
    email: String,
    renew_threshold_days: u32,
    renew_check_interval_hours: u32,
    key_type: KeyType,
    dns_provider: Option<Arc<dyn DnsProvider>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyType {
    Ecdsa,
    Rsa,
}

impl KeyType {
    fn from_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "rsa" => KeyType::Rsa,
            _ => KeyType::Ecdsa,
        }
    }
}

/// Build the blob filename for a given domain and key type.
fn blob_filename(domain: &str, key_type: KeyType) -> String {
    match key_type {
        KeyType::Rsa => format!("{}+rsa", domain),
        KeyType::Ecdsa => domain.to_string(),
    }
}

/// Build the blob content: key PEM + cert chain PEM.
fn build_blob(key_pem: &str, cert_chain_pem: &str) -> String {
    format!("{}\n{}\n", key_pem.trim_end(), cert_chain_pem.trim_end())
}

/// Write a file with 0600 permissions (root:root to match proxy mux).
async fn write_file_0600(path: &Path, content: impl AsRef<[u8]>) -> Result<()> {
    tokio::fs::write(path, content).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

impl AcmeClient {
    /// Create ACME client using native root CAs for TLS verification.
    pub async fn new(
        cert_dir: PathBuf,
        email: String,
        acme_directory: &str,
        renew_threshold_days: u32,
        renew_check_interval_hours: u32,
        key_type: KeyType,
        dns_provider: Option<Arc<dyn DnsProvider>>,
    ) -> Result<Self> {
        // Three-state semantics, locked by the user:
        //   Some(cred) → load and continue
        //   Err(...)   → fail-fast, do NOT overwrite, do NOT re-register
        //   None       → ONLY THEN register a fresh account
        // The `?` propagates the corrupt-file Err, which is exactly what
        // we want — clobbering a half-corrupt file would lose the
        // operator's existing Let's Encrypt account binding.
        let account = match load_account_credentials(&cert_dir).await? {
            Some(cred) => {
                log::info!(
                    "ACME: loaded existing account from {}",
                    cert_dir.join(ACCOUNT_KEY_FILE).display()
                );
                Account::from_credentials(cred).await?
            }
            None => {
                log::info!("ACME: registering new account for {}", email);
                let (account, cred) = Account::create(
                    &instant_acme::NewAccount {
                        contact: &[],
                        terms_of_service_agreed: true,
                        only_return_existing: false,
                    },
                    acme_directory,
                    None,
                )
                .await?;
                tokio::fs::create_dir_all(&cert_dir).await?;
                let cred_json = serde_json::to_string_pretty(&cred)?;
                write_file_0600(&cert_dir.join(ACCOUNT_KEY_FILE), cred_json.as_bytes()).await?;
                log::info!("ACME: account registered, credentials saved");
                account
            }
        };

        Ok(Self {
            account,
            cert_dir,
            email,
            renew_threshold_days,
            renew_check_interval_hours,
            key_type,
            dns_provider,
        })
    }

    /// Create ACME client with a custom HTTP client (allows custom TLS roots, e.g. for Pebble).
    #[allow(clippy::too_many_arguments)]
    pub async fn with_http_client(
        http: Box<dyn instant_acme::HttpClient>,
        cert_dir: PathBuf,
        email: String,
        acme_directory: &str,
        renew_threshold_days: u32,
        renew_check_interval_hours: u32,
        key_type: KeyType,
        dns_provider: Option<Arc<dyn DnsProvider>>,
    ) -> Result<Self> {
        // Same three-state semantics as `new` — see comment there.
        let account = match load_account_credentials(&cert_dir).await? {
            Some(cred) => {
                log::info!(
                    "ACME: loaded existing account from {}",
                    cert_dir.join(ACCOUNT_KEY_FILE).display()
                );
                Account::from_credentials_and_http(cred, http).await?
            }
            None => {
                log::info!("ACME: registering new account for {}", email);
                let (account, cred) = Account::create_with_http(
                    &instant_acme::NewAccount {
                        contact: &[],
                        terms_of_service_agreed: true,
                        only_return_existing: false,
                    },
                    acme_directory,
                    None,
                    http,
                )
                .await?;
                tokio::fs::create_dir_all(&cert_dir).await?;
                let cred_json = serde_json::to_string_pretty(&cred)?;
                write_file_0600(&cert_dir.join(ACCOUNT_KEY_FILE), cred_json.as_bytes()).await?;
                log::info!("ACME: account registered, credentials saved");
                account
            }
        };

        Ok(Self {
            account,
            cert_dir,
            email,
            renew_threshold_days,
            renew_check_interval_hours,
            key_type,
            dns_provider,
        })
    }

    /// Issue a certificate for the given domains.
    /// Returns all (cert_path, key_path) pairs written — one per SAN (identical content).
    pub async fn issue_cert(&self, domains: &[String]) -> Result<Vec<(PathBuf, PathBuf)>> {
        log::info!("ACME: requesting certificate for domains: {:?}", domains);

        let is_wildcard = domains.iter().any(|d| d.starts_with("*."));
        if is_wildcard && self.dns_provider.is_none() {
            anyhow::bail!(
                "wildcard certificate {} requires DNS-01 challenge but no DNS provider is configured",
                domains[0]
            );
        }

        // Build identifier list
        let identifiers: Vec<Identifier> =
            domains.iter().map(|d| Identifier::Dns(d.clone())).collect();

        let new_order = NewOrder {
            identifiers: &identifiers,
        };
        let mut order = self.account.new_order(&new_order).await?;
        log::info!("ACME order created: {}", order.url());

        let authorizations = order.authorizations().await?;
        log::info!("authorizations count: {}", authorizations.len());

        // Determine challenge type
        let use_dns01 = is_wildcard || self.dns_provider.is_some();

        let mut dns_cleanup: Vec<(String, String)> = Vec::new(); // (zone, name) for cleanup

        for auth in &authorizations {
            log::info!("auth status: {:?}", auth.status);

            // Extract identifier string for use in DNS-01 and logging
            let identifier_str = match &auth.identifier {
                Identifier::Dns(s) => s.clone(),
            };

            let challenge = if use_dns01 {
                auth.challenges
                    .iter()
                    .find(|c| c.r#type == ChallengeType::Dns01 && !c.token.is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!("no DNS-01 challenge found for {:?}", auth.identifier)
                    })?
            } else {
                auth.challenges
                    .iter()
                    .find(|c| c.r#type == ChallengeType::Http01 && !c.token.is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!("no HTTP-01 challenge found for {}", identifier_str)
                    })?
            };

            if use_dns01 {
                let key_auth = order.key_authorization(challenge).as_str().to_string();
                let txt_name = format!("_acme-challenge.{}", identifier_str);
                let txt_value = key_auth;

                // Find zone for this identifier
                let (zone, _zone_id) = self
                    .dns_provider
                    .as_ref()
                    .unwrap()
                    .find_zone(&identifier_str)
                    .await?;

                self.dns_provider
                    .as_ref()
                    .unwrap()
                    .create_txt(&zone, &txt_name, &txt_value, 60)
                    .await?;

                dns_cleanup.push((zone.clone(), txt_name.clone()));

                log::info!(
                    "DNS-01 challenge set: {} = {} (zone: {})",
                    txt_name,
                    txt_value,
                    zone
                );

                // Poll for propagation via hickory-resolver
                let propagated = wait_for_txt_propagation(&txt_name, &txt_value, 120, 5).await?;
                if !propagated {
                    log::warn!(
                        "DNS-01 TXT record may not be fully propagated yet, proceeding anyway"
                    );
                }
            } else {
                // HTTP-01
                let key_auth = order.key_authorization(challenge).as_str().to_string();
                self.write_challenge(&challenge.token, &key_auth).await?;
            }

            order.set_challenge_ready(&challenge.url).await?;
        }

        // Poll until order is ready or invalid
        let mut retries = 0u8;
        loop {
            let state = order.state();
            if state.status == OrderStatus::Ready {
                break;
            }
            if state.status == OrderStatus::Invalid {
                anyhow::bail!("ACME order invalid: {:?}", state.error);
            }
            if retries >= 10 {
                anyhow::bail!("ACME order timeout waiting for ready");
            }
            sleep(Duration::from_secs(5)).await;
            order.refresh().await?;
            retries += 1;
        }

        // Generate CSR using openssl (for SEC1/PKCS#1 key output)
        let (key_pem, csr_der) = self.generate_csr(domains)?;

        // Finalize order with CSR
        order.finalize(&csr_der).await?;

        // Poll for certificate
        let mut retries = 0u8;
        let cert_chain_pem = loop {
            if let Some(cert) = order.certificate().await? {
                break cert;
            }
            if retries >= 30 {
                anyhow::bail!("ACME order timeout waiting for certificate");
            }
            sleep(Duration::from_secs(5)).await;
            retries += 1;
        };

        // Build blob content
        let blob = build_blob(&key_pem, &cert_chain_pem);

        // Write blob files: one per SAN (identical content), and wildcard literal filename
        let mut written = Vec::new();

        for domain in domains {
            let filename = blob_filename(domain, self.key_type);
            let path = self.cert_dir.join(&filename);
            write_file_0600(&path, blob.as_bytes())
                .await
                .with_context(|| format!("write blob for {}", domain))?;
            log::info!("ACME blob written: {}", path.display());
            written.push((path.clone(), path)); // cert_path == key_path (blob is combined)
        }

        // Also write wildcard blob with literal `*.` prefix if wildcard
        if is_wildcard {
            // domains should include base domain; we write wildcard blob separately
            for domain in domains {
                if domain.starts_with("*.") {
                    let filename = blob_filename(domain, self.key_type);
                    let path = self.cert_dir.join(&filename);
                    write_file_0600(&path, blob.as_bytes())
                        .await
                        .with_context(|| format!("write wildcard blob for {}", domain))?;
                    log::info!("ACME wildcard blob written: {}", path.display());
                    // Already covered in the main loop, but this handles the wildcard-only case
                }
            }
        }

        // Cleanup HTTP-01 challenge files (DNS-01 cleanup is optional, done at cert expiry)
        for auth in &authorizations {
            let challenge = auth
                .challenges
                .iter()
                .find(|c| c.r#type == ChallengeType::Http01 && !c.token.is_empty());
            if let Some(c) = challenge {
                self.remove_challenge(&c.token).await;
            }
        }

        Ok(written)
    }

    /// Generate a CSR (DER) and private key (PEM) for the given domains.
    /// Key format: SEC1 (P-256) for ECDSA, PKCS#1 for RSA — matches proxy mux / autocert.
    /// Uses openssl directly to avoid rcgen's RSA-generation feature-gate dance.
    fn generate_csr(&self, domains: &[String]) -> Result<(String, Vec<u8>)> {
        use openssl::hash::MessageDigest;
        use openssl::nid::Nid;
        use openssl::x509::{X509Name, X509ReqBuilder};

        let (key_pem, pkey) = match self.key_type {
            KeyType::Ecdsa => {
                let group = openssl::ec::EcGroup::from_curve_name(Nid::X9_62_PRIME256V1)?;
                let ec_key = openssl::ec::EcKey::generate(&group)?;
                let pem = ec_key.private_key_to_pem()?;
                let pkey = openssl::pkey::PKey::from_ec_key(ec_key)?;
                (String::from_utf8(pem)?, pkey)
            }
            KeyType::Rsa => {
                let rsa_key = openssl::rsa::Rsa::generate(2048)?;
                let pem = rsa_key.private_key_to_pem()?;
                let pkey = openssl::pkey::PKey::from_rsa(rsa_key)?;
                (String::from_utf8(pem)?, pkey)
            }
        };

        // Build CSR
        let mut builder = X509ReqBuilder::new()?;
        let mut name = X509Name::builder()?;
        name.append_entry_by_text("CN", &domains[0])?;
        let name = name.build();
        builder.set_subject_name(&name)?;
        builder.set_pubkey(&pkey)?;

        // Add SANs (must include base domain for wildcard certs: ["example.com", "*.example.com"])
        let mut san = openssl::x509::extension::SubjectAlternativeName::new();
        for d in domains {
            san.dns(d);
        }
        let ctx = builder.x509v3_context(None);
        let san_ext = san.build(&ctx)?;
        let mut ext_stack = openssl::stack::Stack::new()?;
        ext_stack.push(san_ext)?;
        builder.add_extensions(&ext_stack)?;

        builder.sign(&pkey, MessageDigest::sha256())?;
        let csr_der = builder.build().to_der()?;

        Ok((key_pem, csr_der))
    }
    async fn write_challenge(&self, token: &str, key_auth: &str) -> Result<()> {
        let challenge_dir = self.cert_dir.join(".well-known/acme-challenge");
        tokio::fs::create_dir_all(&challenge_dir).await?;
        let path = challenge_dir.join(token);
        tokio::fs::write(&path, key_auth).await?;
        log::debug!("ACME challenge written to {}", path.display());
        Ok(())
    }

    async fn remove_challenge(&self, token: &str) {
        let path = self.cert_dir.join(".well-known/acme-challenge").join(token);
        if let Err(e) = tokio::fs::remove_file(&path).await {
            log::warn!("failed to remove challenge file: {}", e);
        }
    }

    /// v2 entry point: issue a cert executing a per-identifier `IssuancePlan`.
    ///
    /// For each SAN in `domains`, the plan picks the challenge type. The
    /// `dns_providers` registry is consulted (by provider name) for the
    /// DNS-01 challenges — so a multi-provider deployment can mix them
    /// per SAN. (Current code path: all DNS-01 SANs share the single
    /// provider named in `plan.dns_provider_name`; future work can extend
    /// to per-SAN provider lookup if needed.)
    /// `IssueTrace` is the per-stage callback `issue_with_plan` emits
    /// to whenever it crosses a boundary that an operator might want
    /// to see in real time — DNS-01 TXT set, DNS propagation polling,
    /// ACME order ready polling, cert-ready polling, blob write.
    /// `ensure_one` wires it up to push each event into the in-memory
    /// `EventBuffer` so the dashboard activity panel surfaces the
    /// trace live. `None` is the test-harness path — no surface to
    /// emit to.
    pub async fn issue_with_plan(
        &self,
        domains: &[String],
        plan: &pangolin_core::IssuancePlan,
        dns_providers: &std::collections::HashMap<String, Arc<dyn DnsProvider>>,
        trace: Option<&IssueTrace>,
    ) -> Result<Vec<(PathBuf, PathBuf)>> {
        if plan.challenges.is_empty() {
            anyhow::bail!("issue_with_plan called with empty plan (auto_issue=false?)");
        }
        if domains.len() != plan.challenges.len() {
            anyhow::bail!(
                "domains/plan length mismatch: {} vs {}",
                domains.len(),
                plan.challenges.len()
            );
        }

        // `emit` is the single channel for "tell the operator
        // something happened" — always logs to stdout, and ALSO
        // pushes to the dashboard EventBuffer when `trace` is Some.
        // Keeping the two paths in one closure makes "spinner stuck,
        // no event" structurally impossible.
        let started = std::time::Instant::now();
        let emit = |stage: &str, detail: String| {
            let elapsed = started.elapsed().as_secs();
            log::info!("ACME {} [{}s] {}", stage, elapsed, detail);
            if let Some(t) = trace {
                t(stage, format!("[{}s] {}", elapsed, detail));
            }
        };

        emit(
            "issue-begin",
            format!(
                "{} SAN(s), {} challenge(s)",
                domains.len(),
                plan.challenges.len()
            ),
        );

        // Build identifier list (must match domains ordering).
        let identifiers: Vec<Identifier> =
            domains.iter().map(|d| Identifier::Dns(d.clone())).collect();
        let new_order = NewOrder {
            identifiers: &identifiers,
        };
        let mut order = self.account.new_order(&new_order).await?;
        emit("order-created", order.url().to_string());

        let authorizations = order.authorizations().await?;
        emit(
            "authz-fetched",
            format!("{} authorization(s)", authorizations.len()),
        );

        // Pick the DNS provider once for the whole order. (All DNS-01
        // SANs in the plan must reference the same provider; the planner
        // enforces this since plan.dns_provider_name is singular.)
        let dns_provider: Option<&Arc<dyn DnsProvider>> = plan
            .dns_provider_name
            .as_ref()
            .and_then(|name| dns_providers.get(name));

        for (i, auth) in authorizations.iter().enumerate() {
            let identifier_str = match &auth.identifier {
                Identifier::Dns(s) => s.clone(),
            };
            let (_, plan_ct) = plan
                .challenges
                .get(i)
                .ok_or_else(|| anyhow::anyhow!("plan missing entry for index {}", i))?;
            // Map our plan's ChallengeType to instant_acme's ChallengeType.
            let acme_ct = match plan_ct {
                pangolin_core::ChallengeType::Dns01 => ChallengeType::Dns01,
                pangolin_core::ChallengeType::Http01 => ChallengeType::Http01,
            };

            let challenge = auth
                .challenges
                .iter()
                .find(|c| c.r#type == acme_ct && !c.token.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!("no {:?} challenge for {}", acme_ct, identifier_str)
                })?;

            match plan_ct {
                pangolin_core::ChallengeType::Dns01 => {
                    let p = dns_provider.ok_or_else(|| {
                        anyhow::anyhow!(
                            "plan requested DNS-01 for {} but provider '{:?}' not in registry",
                            identifier_str,
                            plan.dns_provider_name
                        )
                    })?;
                    let key_auth = order.key_authorization(challenge).as_str().to_string();
                    let txt_name = format!("_acme-challenge.{}", identifier_str);
                    let txt_value = key_auth;
                    emit(
                        "dns-zone",
                        format!("looking up zone for {}", identifier_str),
                    );
                    let (zone, _zone_id) = p.find_zone(&identifier_str).await?;
                    emit("dns-zone", format!("zone={}", zone));
                    // The DNS provider call itself can take seconds
                    // (Cloudflare API, retries) — bracket it so the
                    // operator sees both ends.
                    emit(
                        "dns-set",
                        format!("creating TXT {} (zone={}, ttl=60)", txt_name, zone),
                    );
                    p.create_txt(&zone, &txt_name, &txt_value, 60).await?;
                    emit(
                        "dns-set",
                        format!("TXT {} created (zone={})", txt_name, zone),
                    );
                    // The 120 s propagation wait is the longest synchronous
                    // step in the whole flow — the user's primary
                    // complaint was "stuck here, dashboard silent". Now
                    // we emit both ends.
                    emit(
                        "dns-wait",
                        format!("polling propagation: max 120s, every 5s ({})", txt_name),
                    );
                    let wait_started = std::time::Instant::now();
                    let propagated =
                        wait_for_txt_propagation(&txt_name, &txt_value, 120, 5).await?;
                    let wait_elapsed = wait_started.elapsed().as_secs();
                    if propagated {
                        emit("dns-wait", format!("propagated in {}s", wait_elapsed));
                    } else {
                        emit(
                            "dns-wait",
                            format!(
                                "timeout after {}s — proceeding anyway (may fail upstream)",
                                wait_elapsed
                            ),
                        );
                        log::warn!("DNS-01 TXT may not be fully propagated, proceeding");
                    }
                }
                pangolin_core::ChallengeType::Http01 => {
                    let key_auth = order.key_authorization(challenge).as_str().to_string();
                    self.write_challenge(&challenge.token, &key_auth).await?;
                    emit(
                        "http01",
                        format!("wrote challenge file token={}", challenge.token),
                    );
                }
            }

            order.set_challenge_ready(&challenge.url).await?;
            emit(
                "challenge-ready",
                format!("notified ACME server for {}", identifier_str),
            );
        }

        // Poll until order is ready or invalid.
        emit("order-poll", "starting (max 10 × 5s = 50s)".to_string());
        let mut retries = 0u8;
        loop {
            let state = order.state();
            if state.status == OrderStatus::Ready {
                emit(
                    "order-ready",
                    format!("status=Ready after {} poll(s)", retries),
                );
                break;
            }
            if state.status == OrderStatus::Invalid {
                emit(
                    "order-invalid",
                    format!("status=Invalid error={:?}", state.error),
                );
                anyhow::bail!("ACME order invalid: {:?}", state.error);
            }
            if retries >= 10 {
                emit(
                    "order-timeout",
                    format!(
                        "still {:?} after {} polls, giving up",
                        state.status, retries
                    ),
                );
                anyhow::bail!("ACME order timeout waiting for ready");
            }
            emit(
                "order-poll",
                format!(
                    "attempt {}/10: status={:?}, sleeping 5s",
                    retries + 1,
                    state.status
                ),
            );
            sleep(Duration::from_secs(5)).await;
            order.refresh().await?;
            retries += 1;
        }

        // Generate CSR + finalize.
        let (key_pem, csr_der) = self.generate_csr(domains)?;
        emit("csr", format!("generated for {} SAN(s)", domains.len()));
        order.finalize(&csr_der).await?;
        emit("finalize", "submitted CSR to ACME server".to_string());

        // Poll for certificate.
        emit(
            "cert-poll",
            "waiting for cert chain (max 30 × 5s = 150s)".to_string(),
        );
        let mut retries = 0u8;
        let cert_chain_pem = loop {
            if let Some(cert) = order.certificate().await? {
                emit(
                    "cert-ready",
                    format!("received {} bytes after {} poll(s)", cert.len(), retries),
                );
                break cert;
            }
            if retries >= 30 {
                emit(
                    "cert-timeout",
                    format!("no cert after {} polls, giving up", retries),
                );
                anyhow::bail!("ACME order timeout waiting for certificate");
            }
            if retries.is_multiple_of(3) {
                // Don't spam: emit every 3rd poll (≈15s intervals).
                emit(
                    "cert-poll",
                    format!("attempt {}/30: not ready, sleeping 5s", retries + 1),
                );
            }
            sleep(Duration::from_secs(5)).await;
            retries += 1;
        };

        // Build blob content.
        let blob = build_blob(&key_pem, &cert_chain_pem);

        // Write one blob per SAN (identical content), including the literal
        // `*.example.com` filename for wildcard certs.
        let mut written = Vec::new();
        for domain in domains {
            let filename = blob_filename(domain, self.key_type);
            let path = self.cert_dir.join(&filename);
            write_file_0600(&path, blob.as_bytes())
                .await
                .with_context(|| format!("write blob for {}", domain))?;
            emit("blob-write", format!("{}", path.display()));
            written.push((path.clone(), path));
        }

        // Cleanup HTTP-01 challenge files.
        for auth in &authorizations {
            if let Some(c) = auth
                .challenges
                .iter()
                .find(|c| c.r#type == ChallengeType::Http01 && !c.token.is_empty())
            {
                self.remove_challenge(&c.token).await;
            }
        }

        emit("issue-done", "complete".to_string());
        Ok(written)
    }

    /// Check expiry of existing cert and renew if within threshold.
    /// Returns all (cert_path, key_path) pairs written.
    pub async fn check_and_renew(
        &self,
        domains: &[String],
    ) -> Result<Option<Vec<(PathBuf, PathBuf)>>> {
        // For blob format, check for any existing blob file for this domain
        let filename = blob_filename(&domains[0], self.key_type);
        let blob_path = self.cert_dir.join(&filename);

        if !blob_path.exists() {
            log::info!("no existing cert for {}, issuing new", domains[0]);
            let result = self.issue_cert(domains).await?;
            return Ok(Some(result));
        }

        // Parse expiry from blob
        let blob_content = tokio::fs::read_to_string(&blob_path).await?;
        let expiry = parse_blob_expiry(&blob_content)?;
        let now = Utc::now();
        let days = (expiry - now).num_days();

        log::info!(
            "certificate for {} expires on {} (in {} days)",
            domains[0],
            expiry,
            days
        );

        if days <= self.renew_threshold_days as i64 {
            log::info!(
                "renewing certificate (threshold {} days)",
                self.renew_threshold_days
            );
            let result = self.issue_cert(domains).await?;
            return Ok(Some(result));
        }

        Ok(None)
    }
}

/// Parse certificate expiry from a blob (key_pem + cert chain).
fn parse_blob_expiry(blob: &str) -> Result<DateTime<Utc>> {
    let cert_block = blob
        .split("-----BEGIN CERTIFICATE-----")
        .nth(1)
        .and_then(|s| s.split("-----END CERTIFICATE-----").next())
        .ok_or_else(|| anyhow::anyhow!("no certificate block in blob"))?;

    let der = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        cert_block.trim(),
    )
    .map_err(|e| anyhow::anyhow!("base64 decode error: {}", e))?;

    let (_, cert) = x509_parser::parse_x509_certificate(&der)
        .map_err(|e| anyhow::anyhow!("X509 parse error: {}", e))?;

    let not_after = cert.tbs_certificate.validity.not_after.timestamp();
    DateTime::from_timestamp(not_after, 0).ok_or_else(|| anyhow::anyhow!("invalid timestamp"))
}

/// Richer parse: pulls every piece of metadata the `certs` row wants
/// out of the leaf cert in a blob — `(not_before, not_after, SANs)`.
///
/// Used by [`scan_and_import_blobs`] so the imported row carries the
/// real issuance date (cert's `NotBefore`), real expiry (`NotAfter`),
/// and the actual SAN list (multi-SAN / wildcard / IP) instead of
/// guessing from the filename. A multi-SAN cert like
/// `example.com + www.example.com` imported from disk previously
/// recorded only the filename's domain — operators couldn't tell the
/// row covered `www.` too.
fn parse_blob_metadata(blob: &str) -> Result<(DateTime<Utc>, DateTime<Utc>, Vec<String>)> {
    let cert_block = blob
        .split("-----BEGIN CERTIFICATE-----")
        .nth(1)
        .and_then(|s| s.split("-----END CERTIFICATE-----").next())
        .ok_or_else(|| anyhow::anyhow!("no certificate block in blob"))?;
    let der = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        cert_block.trim(),
    )
    .map_err(|e| anyhow::anyhow!("base64 decode: {}", e))?;
    let (_, cert) = x509_parser::parse_x509_certificate(&der)
        .map_err(|e| anyhow::anyhow!("X509 parse: {}", e))?;
    let not_before = DateTime::from_timestamp(cert.tbs_certificate.validity.not_before.timestamp(), 0)
        .ok_or_else(|| anyhow::anyhow!("invalid NotBefore"))?;
    let not_after = DateTime::from_timestamp(cert.tbs_certificate.validity.not_after.timestamp(), 0)
        .ok_or_else(|| anyhow::anyhow!("invalid NotAfter"))?;

    // Extract SANs from the `SubjectAltName` X.509 extension. Falls back
    // to the cert's CN if no SAN extension is present (very old / hand-
    // rolled certs). We only emit DNS names — IP SANs / email SANs /
    // URI SANs don't map onto our `domain` PK.
    let mut sans: Vec<String> = Vec::new();
    if let Ok(Some(san_ext)) = cert.subject_alternative_name() {
        for gn in &san_ext.value.general_names {
            if let x509_parser::extensions::GeneralName::DNSName(name) = gn {
                sans.push(name.to_string());
            }
        }
    }
    if sans.is_empty() {
        // Fall back to CN. Best-effort: split the subject DN and grab
        // the CN attribute. Wildcard CNs with multi-SAN are rare in
        // modern certs but possible in legacy on-disk files.
        if let Some(cn) = cert
            .subject()
            .iter_common_name()
            .next()
            .and_then(|attr| attr.as_str().ok())
        {
            sans.push(cn.to_string());
        }
    }
    Ok((not_before, not_after, sans))
}

/// Scan `app.cert_manager.cert_dir` for cert blob files and import any
/// whose domain isn't already in the `certs` table.
///
/// Two motivations (both reported by operators):
///
/// 1. Pre-V4 installs and operator-managed deployments may have
///    blob files on disk that were never reflected in the `certs`
///    table. Without import, the admin UI shows an empty cert list
///    even though TLS works.
/// 2. After the rename to `acme_account.json`, a fresh re-registered
///    account will issue NEW blobs alongside the old ones; without
///    a scan, the certs table would only contain the new ones and
///    the old SAN coverage would be invisible.
///
/// Conservative semantics:
/// - **Don't clobber existing rows.** A domain that already appears
///   in `certs` (in any status) is left alone — the operator's manual
///   status, last_error, etc. take precedence over what's on disk.
/// - **`source = 'disk-import'`** distinguishes these rows from
///   manual uploads (`manual`) and ACME-issued (`acme`).
/// - Skips `acme_account.json`, the legacy `acme_account+key`,
///   dotfiles, directories (so `.well-known/` is invisible), and
///   files that don't parse as a cert chain.
///
/// Idempotent — safe to call on every restart. Returns the count of
/// rows actually inserted.
pub async fn scan_and_import_blobs(app: &Arc<App>) -> anyhow::Result<usize> {
    let cert_dir = &app.cert_manager.cert_dir;
    if !cert_dir.exists() {
        return Ok(0);
    }
    let mut entries = match tokio::fs::read_dir(cert_dir).await {
        Ok(e) => e,
        Err(e) => anyhow::bail!("read_dir {}: {}", cert_dir.display(), e),
    };
    let mut imported = 0usize;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.is_dir() {
            continue;
        }
        let filename = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        // Skip the account credentials files (canonical + legacy) and
        // any hidden file (`.DS_Store`, editor swap files, …).
        if filename == ACCOUNT_KEY_FILE
            || filename == "acme_account+key"
            || filename.starts_with('.')
        {
            continue;
        }
        // Derive the domain from the filename. `+rsa` suffix marks
        // the RSA companion blob (same content as the ECDSA file,
        // just a different on-disk format). Either form represents
        // the same logical cert; importing both as the same row is
        // fine because `upsert_cert` is keyed on domain.
        let domain = filename
            .strip_suffix("+rsa")
            .unwrap_or(filename)
            .to_string();

        // Skip if a row already exists — operator-owned, don't touch.
        let existing = {
            let conn = app.db.lock().await;
            pangolin_core::db::get_cert(&conn, &domain).unwrap_or(None)
        };
        if existing.is_some() {
            continue;
        }

        // Read the blob; if it doesn't look like a cert chain, skip
        // (could be random operator file — we don't want to import
        // those as 'issued').
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) => {
                log::warn!("scan: skip {} (read failed: {})", path.display(), e);
                continue;
            }
        };
        if !content.contains("-----BEGIN CERTIFICATE-----") {
            // Not a cert blob — skip silently. The cert_dir holds
            // mixed files (challenge tokens, account JSON, etc.); a
            // log line per skipped file would be noisy.
            continue;
        }
        // Parse the leaf cert for real metadata — NotBefore (used as
        // both `created_at` and `issued_at` so the table reflects when
        // the cert was actually issued, not when we ran the scan),
        // NotAfter (the expiry the operator cares about), and the
        // SAN list (multi-SAN / wildcard coverage that would otherwise
        // be lost). On parse failure: log loudly so operators can
        // diagnose weird blob formats (`-----BEGIN TRUSTED ...`,
        // BOM-prefixed PEM, etc.) instead of silently importing a row
        // with no expiry.
        let (not_before, not_after, sans) = match parse_blob_metadata(&content) {
            Ok(t) => t,
            Err(e) => {
                log::warn!(
                    "scan: {} metadata parse failed: {} — importing row with no expiry/SANs",
                    path.display(),
                    e
                );
                // Conservative fallback: register the existence of the
                // blob anyway so the operator sees the file in /certs;
                // expiry + SANs are blank and a renewal scan can fill
                // them in later.
                (chrono::Utc::now(), chrono::Utc::now(), vec![domain.clone()])
            }
        };

        let cert = pangolin_core::Cert {
            domain: domain.clone(),
            cert_file: path.to_string_lossy().into_owned(),
            key_file: path.to_string_lossy().into_owned(),
            expires_at: Some(not_after),
            created_at: not_before,
            sans,
            // `disk-import` is a distinct source so the admin UI can
            // tell at a glance "this row was reconstructed from a
            // file, not from ACME or a manual upload form".
            source: "disk-import".into(),
            acme_dns_provider: None,
            acme_account_id: None,
            issued_at: not_before.timestamp(),
            status: pangolin_core::CertStatus::Issued,
            started_at: None,
            last_error: None,
        };
        {
            let conn = app.db.lock().await;
            if let Err(e) = pangolin_core::db::upsert_cert(&conn, &cert) {
                log::warn!("scan: upsert {} failed: {}", domain, e);
                continue;
            }
        }
        imported += 1;
        log::info!(
            "scan: imported blob → {} (expires_at={}, SANs={:?})",
            domain,
            not_after.format("%Y-%m-%d"),
            cert.sans,
        );
        app.add_event(pangolin_core::EventType::Info {
            message: format!(
                "scan: imported {} (expires {}, {} SAN(s))",
                domain,
                not_after.format("%Y-%m-%d"),
                cert.sans.len()
            ),
        });
    }
    Ok(imported)
}

// The `mod tests` block below sits before some non-test items (the
// `AcmeState` impl was added in PR-2 after the tests block originally
// appeared). Rather than reshuffle the file, suppress the lint.
#[allow(clippy::items_after_test_module)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_filename_ecdsa() {
        assert_eq!(blob_filename("example.com", KeyType::Ecdsa), "example.com");
    }

    #[test]
    fn blob_filename_rsa() {
        assert_eq!(
            blob_filename("example.com", KeyType::Rsa),
            "example.com+rsa"
        );
    }

    #[test]
    fn blob_filename_wildcard() {
        assert_eq!(
            blob_filename("*.example.com", KeyType::Ecdsa),
            "*.example.com"
        );
    }

    #[test]
    fn key_type_from_str() {
        assert_eq!(KeyType::from_str("ecdsa"), KeyType::Ecdsa);
        assert_eq!(KeyType::from_str("ECDSA"), KeyType::Ecdsa);
        assert_eq!(KeyType::from_str("rsa"), KeyType::Rsa);
        assert_eq!(KeyType::from_str("RSA"), KeyType::Rsa);
        assert_eq!(KeyType::from_str("foo"), KeyType::Ecdsa); // default
    }

    #[test]
    fn build_blob_contains_key_and_cert() {
        let key_pem = "-----BEGIN EC PRIVATE KEY-----\nMHQCAQEE\n-----END EC PRIVATE KEY-----";
        let cert_pem = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----";
        let blob = build_blob(key_pem, cert_pem);
        assert!(blob.contains("-----BEGIN EC PRIVATE KEY-----"));
        assert!(blob.contains("-----BEGIN CERTIFICATE-----"));
    }

    #[test]
    fn blob_starts_with_ec_private_key() {
        let key_pem = "-----BEGIN EC PRIVATE KEY-----\nMHQCAQEE\n-----END EC PRIVATE KEY-----";
        let cert_pem = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n-----BEGIN CERTIFICATE-----\nMIIB2\n-----END CERTIFICATE-----";
        let blob = build_blob(key_pem, cert_pem);
        assert!(blob.starts_with("-----BEGIN EC PRIVATE KEY-----"));
        assert!(blob.contains("-----BEGIN CERTIFICATE-----"));
    }

    #[test]
    fn blob_starts_with_rsa_private_key() {
        let key_pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIBog\n-----END RSA PRIVATE KEY-----";
        let cert_pem = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----";
        let blob = build_blob(key_pem, cert_pem);
        assert!(blob.starts_with("-----BEGIN RSA PRIVATE KEY-----"));
        assert!(blob.contains("-----BEGIN CERTIFICATE-----"));
    }

    // ──────────────────────────────────────────────────────────────────
    // ACME account credentials loader (issue #45 follow-up).
    // ──────────────────────────────────────────────────────────────────

    use std::path::Path;

    fn parse_err_message(content: &str, path: &Path) -> String {
        match parse_account_credentials(content, path) {
            Ok(_) => panic!("expected parse error"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn parse_account_credentials_pem_detected() {
        // Most common cause of the historical 'invalid number at line 1
        // column 2': operator clobbered the file with a PEM. Error
        // must name the file, hint at the cause, and tell the operator
        // to MANUALLY move it aside — the code itself must never
        // overwrite or auto-re-register (user policy).
        //
        // Path comes from `tempdir()` rather than a hardcoded string so
        // the test is independent of the operator's deploy layout
        // (`/opt/pangolin/certs/...` is one possible layout, not the
        // contract).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(ACCOUNT_KEY_FILE);
        let pem = "-----BEGIN PRIVATE KEY-----\nMIIE...\n-----END PRIVATE KEY-----";
        let msg = parse_err_message(pem, &path);
        let path_display = path.display().to_string();
        assert!(msg.contains(&path_display), "must name file path: {msg}");
        assert!(msg.contains("PEM"), "{msg}");
        assert!(
            msg.contains("mv "),
            "must hint at the mv command, got: {msg}"
        );
        // The hint mentions that AFTER the operator moves the file, a
        // fresh account will be registered. That's instruction text,
        // not auto-behaviour — the loader still propagates Err and
        // the caller refuses to overwrite. The behaviour assertion is
        // in `load_account_credentials_errors_on_corrupt_canonical`.
    }

    #[test]
    fn parse_account_credentials_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(ACCOUNT_KEY_FILE);
        let msg = parse_err_message("", &path);
        let path_display = path.display().to_string();
        assert!(msg.contains("empty"), "{msg}");
        assert!(msg.contains(&path_display), "must name file path: {msg}");
    }

    #[test]
    fn parse_account_credentials_garbled_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(ACCOUNT_KEY_FILE);
        let msg = parse_err_message("not really json", &path);
        let path_display = path.display().to_string();
        assert!(msg.contains(&path_display), "must name file path: {msg}");
        assert!(
            msg.contains("not really json") || msg.contains("First bytes"),
            "{msg}"
        );
    }

    #[tokio::test]
    async fn load_account_credentials_none_when_dir_missing() {
        let dir = tempfile::tempdir().unwrap();
        let res = load_account_credentials(dir.path()).await.unwrap();
        // `unwrap` is OK because Ok(None) flattens via `assert!(is_none())`.
        assert!(
            res.is_none(),
            "no file → Ok(None) so caller can register fresh"
        );
    }

    #[tokio::test]
    async fn load_account_credentials_errors_on_corrupt_canonical() {
        // Locked by user: corrupt file must propagate as Err. The caller
        // (AcmeClient::new) must NOT catch this and re-register, which
        // would silently lose the operator's existing Let's Encrypt
        // account binding.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(ACCOUNT_KEY_FILE);
        tokio::fs::write(
            &path,
            "-----BEGIN PRIVATE KEY-----\nbad\n-----END PRIVATE KEY-----",
        )
        .await
        .unwrap();
        // `AccountCredentials` doesn't implement Debug, so `unwrap_err`
        // won't compile. Match the Result explicitly instead.
        let msg = match load_account_credentials(dir.path()).await {
            Ok(_) => panic!("corrupt file must Err, not Ok"),
            Err(e) => e.to_string(),
        };
        assert!(msg.contains("PEM"), "PEM detection must fire: {msg}");
        assert!(msg.contains(ACCOUNT_KEY_FILE), "must name the file: {msg}");
    }

    #[tokio::test]
    async fn load_account_credentials_ignores_non_canonical_files() {
        // Locked by user: only `acme_account.json` participates in the
        // loader. The historical `acme_account+key`, a stray PEM, or
        // any other `acme_account*` file in `cert_dir` is invisible
        // — the loader returns `None` (caller registers fresh) rather
        // than reading them, migrating them, or warning about them.
        let dir = tempfile::tempdir().unwrap();
        // Drop several files that previous code paths or operators
        // might have left behind. None of them should be honoured.
        for noise in [
            "acme_account+key",
            "acme_account.key",
            "acme_account.pem",
            "acme_account.bak",
        ] {
            tokio::fs::write(dir.path().join(noise), "garbage")
                .await
                .unwrap();
        }
        let res = load_account_credentials(dir.path()).await.unwrap();
        // `None`, not `Err` — the noise files are simply not seen.
        assert!(
            res.is_none(),
            "non-canonical files must not be read or trigger an error"
        );
    }
}

// ---------------------------------------------------------------------------
// AcmeState — runtime ACME orchestration (v2)
// ---------------------------------------------------------------------------

use std::collections::HashMap;
use tokio::sync::RwLock;

use pangolin_core::{App, Domain};

/// Runtime state for ACME: holds the `Arc<dyn DnsProvider>` registry (loaded
/// from the `dns_providers` table) and exposes the startup scan + renew loop.
///
/// One `AcmeState` is built in `main()` after the `App` is constructed and
/// shared via `Arc<AcmeState>`. The registry is rebuilt on `reload()` after
/// any admin write (DNS provider add/edit/delete, domain add/edit).
pub struct AcmeState {
    /// DNS provider instances: name → trait object. The trait object
    /// is what's needed to call `create_txt` / `find_zone` at issuance time.
    pub dns_providers: RwLock<HashMap<String, Arc<dyn DnsProvider>>>,
    /// The AcmeClient. Built lazily on first issuance; reused for renew.
    pub client: RwLock<Option<Arc<AcmeClient>>>,
    /// Back-reference to the shared `App` so the `CertRetrier` impl can
    /// drive an out-of-band issuance from an admin HTTP handler without
    /// the caller passing `app` in. `None` until `install_on(app)` runs.
    app: RwLock<Option<Arc<App>>>,
}

impl AcmeState {
    /// Create an empty state (no DNS providers yet, no client). Call
    /// `reload(app)` to populate from the DB.
    pub fn empty() -> Self {
        Self {
            dns_providers: RwLock::new(HashMap::new()),
            client: RwLock::new(None),
            app: RwLock::new(None),
        }
    }

    /// Install this `AcmeState` as the [`pangolin_core::CertRetrier`] on
    /// the given `App` and stash a back-reference so the retrier method
    /// can drive an issuance without the caller passing `app` in.
    /// Called once at process startup from `ngx::main`.
    pub async fn install_on(self: &Arc<Self>, app: &Arc<App>) {
        *self.app.write().await = Some(app.clone());
        app.set_cert_retrier(self.clone() as Arc<dyn pangolin_core::CertRetrier>)
            .await;
    }

    /// Rebuild the DNS provider registry from the DB. Cheap — re-reads
    /// the `dns_providers` table and re-constructs trait objects.
    pub async fn reload(&self, app: &App) -> anyhow::Result<()> {
        let conn = app.db.lock().await;
        let providers = pangolin_core::db::list_dns_providers(&conn).unwrap_or_default();
        drop(conn);

        let mut new_registry = HashMap::new();
        for p in providers.iter().filter(|p| p.enabled) {
            match crate::dns::from_kind_config(p.kind, &p.config) {
                Ok(provider) => {
                    new_registry.insert(p.name.clone(), provider);
                }
                Err(e) => {
                    log::error!(
                        "skipping dns_provider '{}' (kind={}): {}",
                        p.name,
                        p.kind,
                        e
                    );
                    app.add_event(pangolin_core::EventType::CertIssuanceSkipped {
                        domain: p.name.clone(),
                        reason: format!("dns provider factory error: {e}"),
                    });
                }
            }
        }
        *self.dns_providers.write().await = new_registry;
        Ok(())
    }

    /// Build the AcmeClient (if not already built) and return an Arc clone.
    pub async fn client(&self, app: &App) -> anyhow::Result<Arc<AcmeClient>> {
        if let Some(c) = self.client.read().await.as_ref() {
            return Ok(c.clone());
        }
        let mut guard = self.client.write().await;
        if let Some(c) = guard.as_ref() {
            return Ok(c.clone());
        }
        let dns_provider = {
            let reg = self.dns_providers.read().await;
            // Pick the first registered provider as the "default" for the
            // AcmeClient. (The v2 path actually uses the per-plan registry
            // lookup; this single provider is only here for the legacy
            // issue_cert() entry point used in tests.)
            reg.values().next().cloned()
        };
        let cert_dir = std::path::PathBuf::from(&app.config.acme.cert_dir);
        let key_type = match app.config.acme.key_type.as_str() {
            "rsa" => KeyType::Rsa,
            _ => KeyType::Ecdsa,
        };
        let client = AcmeClient::new(
            cert_dir,
            app.config.acme.email.clone(),
            &app.config.acme.acme_directory,
            app.config.acme.renew_threshold_days,
            app.config.acme.renew_check_interval_hours,
            key_type,
            dns_provider,
        )
        .await?;
        let arc = Arc::new(client);
        *guard = Some(arc.clone());
        Ok(arc)
    }

    /// Enumerate `domains` with `auto_issue=true` and ensure a cert blob
    /// exists on disk. If a cert exists, check expiry and renew if within
    /// threshold. If it doesn't exist, issue fresh.
    pub async fn ensure_certs(&self, app: &App) {
        let conn = app.db.lock().await;
        let domains = pangolin_core::db::list_domains(&conn).unwrap_or_default();
        let dns_index_snapshot = app.dns_index.read().await.clone();
        let dns_index = dns_index_snapshot;
        drop(conn);

        for d in domains
            .iter()
            .filter(|d| d.enabled && d.auto_issue)
            .cloned()
            .collect::<Vec<_>>()
        {
            if let Err(e) = self.ensure_one(app, &d, &dns_index).await {
                log::error!("ensure_certs({}): {}", d.domain, e);
                app.add_event(pangolin_core::EventType::CertIssuanceSkipped {
                    domain: d.domain.clone(),
                    reason: e.to_string(),
                });
            }
        }
    }

    pub(crate) async fn ensure_one(
        &self,
        app: &App,
        domain: &Domain,
        dns_index: &pangolin_core::DnsIndex,
    ) -> anyhow::Result<()> {
        // Per-stage trace helper: log to stdout AND push to the
        // EventBuffer so the dashboard's activity feed surfaces the
        // same trace operators see in the log. Keeping the two emit
        // paths in one helper avoids the temptation to call only one
        // (which is exactly how 'spinner stuck forever, no log line'
        // happens — issue #45 follow-up).
        let trace = |stage: &str, detail: String| {
            log::info!("ACME[{}] {}: {}", domain.domain, stage, detail);
            app.add_event(pangolin_core::EventType::Info {
                message: format!("ACME[{}] {}: {}", domain.domain, stage, detail),
            });
        };

        trace("start", format!("auto_issue=true san={}", domain.domain));

        // Build the SAN list: for a wildcard, include the base domain too
        // (browsers won't trust a `*.example.com` cert without the base).
        let sans: Vec<String> = if let Some(base) = domain.domain.strip_prefix("*.") {
            vec![base.to_string(), domain.domain.clone()]
        } else {
            vec![domain.domain.clone()]
        };

        // Issue #45 phase-1 (status transitions): seed the row if missing
        // (catches legacy auto_issue domains that pre-date V4), then plan.
        {
            let conn = app.db.lock().await;
            let _ = pangolin_core::db::ensure_pending_cert_row(
                &conn,
                &domain.domain,
                &app.cert_manager.cert_dir,
            );
        }

        let plan = match pangolin_core::plan_issuance(&sans, domain, dns_index) {
            Ok(p) => {
                trace(
                    "plan",
                    format!(
                        "{} challenge(s), dns_provider={:?}",
                        p.challenges.len(),
                        p.dns_provider_name
                    ),
                );
                p
            }
            Err(e) => {
                // Wildcard without DNS association: surface this as a
                // visible `Skipped` row with the reason so the operator
                // can act on it from the admin UI instead of having to
                // tail the log. `Skipped` is distinct from `Failed`
                // because retrying without fixing the DNS config will
                // not help.
                let reason = e.to_string();
                trace("skipped", reason.clone());
                log::warn!("skipping {} (auto_issue=true): {}", domain.domain, e);
                let conn = app.db.lock().await;
                let _ = pangolin_core::db::set_cert_status_atomic(
                    &conn,
                    &domain.domain,
                    pangolin_core::CertStatus::Skipped,
                    Some(&reason),
                    None,
                );
                drop(conn);
                return Ok(());
            }
        };

        // Check existing cert.
        let cert_path = app.cert_manager.cert_dir.join(&sans[0]);
        let needs_issue = if cert_path.exists() {
            match parse_blob_expiry(&tokio::fs::read_to_string(&cert_path).await?) {
                Ok(expiry) => {
                    let days = (expiry - chrono::Utc::now()).num_days();
                    let need = days <= app.config.acme.renew_threshold_days as i64;
                    trace(
                        "expiry-check",
                        format!(
                            "existing cert expires in {}d (threshold {}d) — {}",
                            days,
                            app.config.acme.renew_threshold_days,
                            if need { "will renew" } else { "skip" }
                        ),
                    );
                    need
                }
                Err(_) => {
                    trace(
                        "expiry-check",
                        "existing cert unreadable — will issue".into(),
                    );
                    true
                }
            }
        } else {
            trace("expiry-check", "no existing cert blob — will issue".into());
            true
        };

        if !needs_issue {
            return Ok(());
        }

        // Transition Pending → Issuing (or anything else → Issuing) with
        // a fresh `started_at` so the UI's "x seconds ago" relative
        // timestamp reflects this attempt.
        {
            let conn = app.db.lock().await;
            let _ = pangolin_core::db::set_cert_status_atomic(
                &conn,
                &domain.domain,
                pangolin_core::CertStatus::Issuing,
                None,
                Some(chrono::Utc::now()),
            );
        }
        trace("transition", "Pending/Failed → Issuing".into());

        let client = self.client(app).await?;
        let dns_providers = self.dns_providers.read().await.clone();
        trace(
            "acme-call",
            format!(
                "issue_with_plan sans={:?} provider={:?}",
                sans, plan.dns_provider_name
            ),
        );
        let issue_started = chrono::Utc::now();
        // Build an Arc'd trace closure to hand down into `issue_with_plan`
        // so its per-stage emits (DNS-01 set, propagation wait, order
        // poll, cert poll, blob write) land in the same EventBuffer
        // operators read on the dashboard. The closure clones `events`
        // (an Arc internally) so it can outlive this stack frame —
        // `issue_with_plan` holds it across many awaits.
        let events_for_trace = app.events.clone();
        let domain_for_trace = domain.domain.clone();
        let inner_trace: IssueTrace = Arc::new(move |stage: &str, detail: String| {
            let line = format!("ACME[{}] {}: {}", domain_for_trace, stage, detail);
            log::info!("{}", line);
            events_for_trace.push(pangolin_core::Event::new(pangolin_core::EventType::Info {
                message: line,
            }));
        });
        match client
            .issue_with_plan(&sans, &plan, &dns_providers, Some(&inner_trace))
            .await
        {
            Ok(written) => {
                let elapsed = (chrono::Utc::now() - issue_started).num_seconds();
                trace(
                    "acme-call",
                    format!("ok in {}s ({} blob path(s))", elapsed, written.len()),
                );
                // Persist the on-disk cert blob path in the certs row so
                // /certs reflects the actual location, then flip to
                // `Issued`. `last_error` is cleared by passing None.
                let blob_path = written
                    .first()
                    .map(|(p, _)| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| {
                        app.cert_manager
                            .cert_dir
                            .join(&domain.domain)
                            .to_string_lossy()
                            .into_owned()
                    });
                let issued_at = chrono::Utc::now();
                // Parse expiry from the freshly issued blob so the certs
                // row shows the right "Expires" column without waiting
                // for a renewal scan.
                let expires_at = tokio::fs::read_to_string(&blob_path)
                    .await
                    .ok()
                    .and_then(|blob| parse_blob_expiry(&blob).ok());
                let conn = app.db.lock().await;
                let existing = pangolin_core::db::get_cert(&conn, &domain.domain).unwrap_or(None);
                let created_at = existing.as_ref().map(|c| c.created_at).unwrap_or(issued_at);
                let started_at = existing.as_ref().and_then(|c| c.started_at);
                let cert_row = pangolin_core::Cert {
                    domain: domain.domain.clone(),
                    cert_file: blob_path.clone(),
                    key_file: blob_path,
                    expires_at,
                    created_at,
                    sans: sans.clone(),
                    source: "acme".into(),
                    acme_dns_provider: plan.dns_provider_name.clone(),
                    acme_account_id: None,
                    issued_at: issued_at.timestamp(),
                    status: pangolin_core::CertStatus::Issued,
                    started_at,
                    last_error: None,
                };
                let _ = pangolin_core::db::upsert_cert(&conn, &cert_row);
                drop(conn);
                trace(
                    "issued",
                    format!(
                        "blob persisted, expires_at={:?}",
                        expires_at.map(|d| d.format("%Y-%m-%d").to_string())
                    ),
                );
                app.add_event(pangolin_core::EventType::CertIssued {
                    domain: domain.domain.clone(),
                });
                Ok(())
            }
            Err(e) => {
                let elapsed = (chrono::Utc::now() - issue_started).num_seconds();
                // Surface the failure in the certs table so the operator
                // can see it without grepping logs. The bubbled-up error
                // still drives the existing event buffer + CertRenewFailed
                // event so renew callers and dashboards see both.
                let err_msg = e.to_string();
                trace("failed", format!("after {}s: {}", elapsed, err_msg));
                let conn = app.db.lock().await;
                let _ = pangolin_core::db::set_cert_status_atomic(
                    &conn,
                    &domain.domain,
                    pangolin_core::CertStatus::Failed,
                    Some(&err_msg),
                    None,
                );
                drop(conn);
                Err(e)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Service integration
// ---------------------------------------------------------------------------

/// `CertRetrier` bridge from the admin UI's `POST /certs/retry` to
/// `AcmeState::ensure_one`. Looks up the domain row, stamps
/// `started_at=now` so the UI's relative-time column reflects this
/// attempt even if `ensure_one` itself is slow (or returns `Skipped`),
/// and forwards to the same renewal-scan code path so failures /
/// transitions land in the same place.
#[async_trait::async_trait]
impl pangolin_core::CertRetrier for AcmeState {
    async fn retry(&self, domain: &str) -> anyhow::Result<()> {
        let app = self
            .app
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow::anyhow!("AcmeState not installed on App; cannot retry"))?;

        // Load the domain row (we need its auto_issue / dns_provider for
        // `plan_issuance`).
        let (domain_row, dns_index_snapshot) = {
            let conn = app.db.lock().await;
            let row = pangolin_core::db::get_domain(&conn, domain)
                .map_err(|e| anyhow::anyhow!("db lookup failed for {}: {}", domain, e))?;
            drop(conn);
            let dns_index = app.dns_index.read().await.clone();
            (row, dns_index)
        };

        let d = match domain_row {
            Some(d) => d,
            None => anyhow::bail!("domain {} not found", domain),
        };
        if !d.auto_issue {
            anyhow::bail!(
                "domain {} does not have auto_issue enabled; \
                 enable it on the Domains page before retrying",
                domain
            );
        }

        // Bump `started_at` so the UI's "x seconds ago" column moves
        // the moment the operator clicks ↻ — even before `ensure_one`
        // has a chance to transition the row to `Issuing`.
        {
            let conn = app.db.lock().await;
            let _ = pangolin_core::db::set_cert_status_atomic(
                &conn,
                domain,
                pangolin_core::CertStatus::Pending,
                None,
                Some(chrono::Utc::now()),
            );
        }

        self.ensure_one(&app, &d, &dns_index_snapshot).await
    }
}

/// Long-running ACME renewal loop, run by `runtime::Service`.
pub struct AcmeService {
    state: Arc<AcmeState>,
}

impl AcmeService {
    /// Build the service. The initial DNS provider load and startup
    /// cert scan happen inside [`Service::run`] so that any failure
    /// aborts process startup (fail-fast) rather than being logged
    /// and ignored.
    pub fn new(state: Arc<AcmeState>) -> Self {
        Self { state }
    }
}

#[async_trait::async_trait]
impl crate::runtime::Service for AcmeService {
    fn name(&self) -> &'static str {
        "acme"
    }

    async fn run(&self, ctx: crate::runtime::ServiceContext) -> anyhow::Result<()> {
        let app = ctx.app.clone();
        let state = self.state.clone();
        let interval_hours = app.config.acme.renew_check_interval_hours.max(1);

        // Initial load + scan. Errors here fail startup.
        state
            .reload(&app)
            .await
            .map_err(|e| anyhow::anyhow!("acme initial reload: {e}"))?;
        state.ensure_certs(&app).await;

        let interval = std::time::Duration::from_secs(interval_hours as u64 * 3600);
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick fires immediately; skip it to avoid running
        // the scan twice at startup.
        ticker.tick().await;

        loop {
            tokio::select! {
                // Bias toward shutdown so a Ctrl-C during a long
                // renewal check doesn't have to wait for the next
                // tick boundary.
                biased;
                _ = ctx.shutdown.cancelled() => {
                    log::info!("ACME: shutdown requested, exiting renewal loop");
                    return Ok(());
                }
                _ = ticker.tick() => {
                    log::info!("ACME: periodic renewal scan (interval={}h)", interval_hours);
                }
                _ = app.dns_change_notify.notified() => {
                    log::info!("ACME: DNS config changed, reloading and re-scanning");
                    if let Err(e) = state.reload(&app).await {
                        log::error!("acme reload after notify: {e}");
                    }
                }
            }
            state.ensure_certs(&app).await;
        }
    }
}
