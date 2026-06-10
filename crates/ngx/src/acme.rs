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

/// Account credentials file — instant-acme AccountCredentials JSON.
/// Named `acme_account+key` to match autocert DirCache convention.
const ACCOUNT_KEY_FILE: &str = "acme_account+key";

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
        let account_key_path = cert_dir.join(ACCOUNT_KEY_FILE);

        let account = if account_key_path.exists() {
            log::info!(
                "loading existing ACME account from {}",
                account_key_path.display()
            );
            let cred_json = tokio::fs::read_to_string(&account_key_path).await?;
            let cred: AccountCredentials = serde_json::from_str(&cred_json)
                .map_err(|e| anyhow::anyhow!("invalid account credentials: {}", e))?;
            Account::from_credentials(cred).await?
        } else {
            log::info!("registering new ACME account for {}", email);
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
            write_file_0600(&account_key_path, cred_json.as_bytes()).await?;
            log::info!("ACME account registered, credentials saved");
            account
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
        let account_key_path = cert_dir.join(ACCOUNT_KEY_FILE);

        let account = if account_key_path.exists() {
            log::info!(
                "loading existing ACME account from {}",
                account_key_path.display()
            );
            let cred_json = tokio::fs::read_to_string(&account_key_path).await?;
            let cred: AccountCredentials = serde_json::from_str(&cred_json)
                .map_err(|e| anyhow::anyhow!("invalid account credentials: {}", e))?;
            Account::from_credentials_and_http(cred, http).await?
        } else {
            log::info!("registering new ACME account for {}", email);
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
            write_file_0600(&account_key_path, cred_json.as_bytes()).await?;
            log::info!("ACME account registered, credentials saved");
            account
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

    /// Start background renewal loop.
    pub fn start_background_renewal(self: Arc<Self>, domains: Vec<String>) {
        let client = self;
        tokio::spawn(async move {
            let interval = Duration::from_secs(3600 * client.renew_check_interval_hours as u64);
            loop {
                log::info!("ACME renewal check for {:?}", domains);
                match client.check_and_renew(&domains).await {
                    Ok(Some(certs)) => {
                        log::info!("ACME renewal succeeded for {:?}", domains);
                        let _ = certs;
                        // TODO: restart pangolin-ngx via systemctl to reload TLS
                    }
                    Ok(None) => log::info!("ACME cert still valid"),
                    Err(e) => log::error!("ACME renewal failed: {}", e),
                }
                sleep(interval).await;
            }
        });
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
}
