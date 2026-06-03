//! ACME certificate management — issue and renew TLS certificates via Let's Encrypt.
//!
//! Uses HTTP-01 challenge for domain verification. The challenge response
//! is served by the admin HTTP server at `/.well-known/acme-challenge/<token>`.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use instant_acme::{Account, AccountCredentials, ChallengeType, Identifier, NewOrder, OrderStatus};
use tokio::sync::RwLock;
use tokio::time::sleep;

/// Account credentials file stored in cert_dir.
const ACCOUNT_KEY_FILE: &str = "account_key.pem";

/// ACME client for issuing and renewing certificates.
pub struct AcmeClient {
    account: Account,
    cert_dir: PathBuf,
    #[allow(dead_code)]
    email: String, // stored for potential future ACME account metadata
    renew_threshold_days: u32,
    renew_check_interval_hours: u32,
}

impl AcmeClient {
    /// Create ACME client using native root CAs for TLS verification.
    pub async fn new(
        cert_dir: PathBuf,
        email: String,
        acme_directory: &str,
        renew_threshold_days: u32,
        renew_check_interval_hours: u32,
    ) -> anyhow::Result<Self> {
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
            tokio::fs::write(&account_key_path, cred_json).await?;
            log::info!("ACME account registered, credentials saved");
            account
        };

        Ok(Self {
            account,
            cert_dir,
            email,
            renew_threshold_days,
            renew_check_interval_hours,
        })
    }

    /// Create ACME client with a custom HTTP client (allows custom TLS roots, e.g. for Pebble).
    pub async fn with_http_client(
        http: Box<dyn instant_acme::HttpClient>,
        cert_dir: PathBuf,
        email: String,
        acme_directory: &str,
        renew_threshold_days: u32,
        renew_check_interval_hours: u32,
    ) -> anyhow::Result<Self> {
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
            tokio::fs::write(&account_key_path, cred_json).await?;
            log::info!("ACME account registered, credentials saved");
            account
        };

        Ok(Self {
            account,
            cert_dir,
            email,
            renew_threshold_days,
            renew_check_interval_hours,
        })
    }

    /// Issue a certificate for the given domains.
    pub async fn issue_cert(&self, domains: &[String]) -> anyhow::Result<(PathBuf, PathBuf)> {
        log::info!("ACME: requesting certificate for domains: {:?}", domains);

        let identifiers: Vec<Identifier> =
            domains.iter().map(|d| Identifier::Dns(d.clone())).collect();

        let new_order = NewOrder {
            identifiers: &identifiers,
        };
        let mut order = self.account.new_order(&new_order).await?;
        log::info!("ACME order created: {}", order.url());

        let authorizations = order.authorizations().await?;
        log::info!("authorizations count: {}", authorizations.len());
        if let Some(auth) = authorizations.first() {
            log::info!("auth status: {:?}", auth.status);
            log::info!("auth challenges count: {}", auth.challenges.len());
            for (i, c) in auth.challenges.iter().enumerate() {
                log::info!(
                    "  challenge[{}]: type={:?} url={} token={}",
                    i,
                    c.r#type,
                    c.url,
                    c.token
                );
            }
        }
        let auth = authorizations
            .first()
            .ok_or_else(|| anyhow::anyhow!("no authorization in order"))?;

        log::info!("auth: {:?}", auth);
        log::info!("challenges: {:?}", auth.challenges);

        let challenge = auth
            .challenges
            .iter()
            .filter(|c| !c.token.is_empty())
            .find(|c| c.r#type == ChallengeType::Http01)
            .ok_or_else(|| anyhow::anyhow!("no HTTP-01 challenge found"))?;

        let key_auth = order.key_authorization(challenge).as_str().to_string();
        self.write_challenge(&challenge.token, &key_auth).await?;

        order.set_challenge_ready(&challenge.url).await?;

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

        // Generate CSR using rcgen
        let (csr_der, key_pem) = self.generate_csr(domains)?;

        // Finalize order with CSR
        order.finalize(&csr_der).await?;

        // Poll for certificate
        let mut retries = 0u8;
        let cert_pem = loop {
            if let Some(cert) = order.certificate().await? {
                break cert;
            }
            if retries >= 30 {
                anyhow::bail!("ACME order timeout waiting for certificate");
            }
            sleep(Duration::from_secs(5)).await;
            retries += 1;
        };

        // Save cert + key
        let host_dir = self.cert_dir.join(&domains[0]);
        tokio::fs::create_dir_all(&host_dir).await?;
        let cert_path = host_dir.join("fullchain.pem");
        let key_path = host_dir.join("privkey.pem");
        tokio::fs::write(&cert_path, &cert_pem).await?;
        tokio::fs::write(&key_path, &key_pem).await?;

        log::info!(
            "ACME certificate saved: {} / {}",
            cert_path.display(),
            key_path.display()
        );

        self.remove_challenge(&challenge.token).await;
        Ok((cert_path, key_path))
    }

    /// Generate a CSR (DER) and private key (PEM) for the given domains.
    fn generate_csr(&self, domains: &[String]) -> anyhow::Result<(Vec<u8>, String)> {
        use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType};

        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, domains[0].as_str());
        params.subject_alt_names = domains
            .iter()
            .skip(1)
            .map(|d| SanType::DnsName(d.as_str().try_into().unwrap()))
            .collect();

        let key_pair = KeyPair::generate_for(&rcgen::PKCS_RSA_SHA256)?;
        let csr = params.serialize_request(&key_pair)?;
        let csr_der = csr.der().to_vec();
        let key_pem = key_pair.serialize_pem();
        Ok((csr_der, key_pem))
    }

    async fn write_challenge(&self, token: &str, key_auth: &str) -> anyhow::Result<()> {
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
    pub async fn check_and_renew(
        &self,
        domains: &[String],
    ) -> anyhow::Result<Option<(PathBuf, PathBuf)>> {
        let host_dir = self.cert_dir.join(&domains[0]);
        let cert_path = host_dir.join("fullchain.pem");
        let key_path = host_dir.join("privkey.pem");

        if !cert_path.exists() || !key_path.exists() {
            log::info!("no existing cert for {}, issuing new", domains[0]);
            let result = self.issue_cert(domains).await?;
            return Ok(Some(result));
        }

        let cert_pem = tokio::fs::read_to_string(&cert_path).await?;
        let expiry = parse_cert_expiry(&cert_pem)?;
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
                    Ok(Some((cert, key))) => {
                        log::info!("ACME renewal succeeded for {:?}", domains);
                        let _ = (cert, key);
                        // TODO: reload TLS in proxy service
                    }
                    Ok(None) => log::info!("ACME cert still valid"),
                    Err(e) => log::error!("ACME renewal failed: {}", e),
                }
                sleep(interval).await;
            }
        });
    }
}

/// Parse certificate expiry from PEM certificate.
fn parse_cert_expiry(pem_str: &str) -> anyhow::Result<DateTime<Utc>> {
    let pem_block = pem::parse(pem_str.as_bytes())
        .map_err(|e| anyhow::anyhow!("failed to parse cert PEM: {}", e))?;
    let der = pem_block.contents();

    let (_, cert) = x509_parser::parse_x509_certificate(der)
        .map_err(|e| anyhow::anyhow!("X509 parse error: {}", e))?;

    let not_after = cert.tbs_certificate.validity.not_after.timestamp();
    DateTime::from_timestamp(not_after, 0).ok_or_else(|| anyhow::anyhow!("invalid timestamp"))
}

// ---- CertManager (exposed to main.rs) ----

/// TLS certificate manager for ngx main.rs.
/// Handles both ACME auto-renewal and manual file-based certs.
pub struct CertManager {
    pub enabled: bool,
    pub cert_dir: PathBuf,
    pub email: String,
    pub acme_directory: String,
    pub renew_threshold_days: u32,
    pub renew_check_interval_hours: u32,
    pub renew_max_retries: u32,
    acme_client: RwLock<Option<Arc<AcmeClient>>>,
}

impl CertManager {
    /// Create a new CertManager.
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
            acme_client: RwLock::new(None),
        }
    }

    /// Initialize ACME client if autorenew is enabled.
    pub async fn init(&self) -> anyhow::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let client = Arc::new(
            AcmeClient::new(
                self.cert_dir.clone(),
                self.email.clone(),
                &self.acme_directory,
                self.renew_threshold_days,
                self.renew_check_interval_hours,
            )
            .await?,
        );
        *self.acme_client.write().await = Some(client.clone());
        Ok(())
    }

    /// Resolve cert and key paths for a given host.
    pub fn resolve_cert(&self, host: &str) -> anyhow::Result<(String, String)> {
        let host_dir = self.cert_dir.join(host);
        let cert = host_dir.join("fullchain.pem");
        let key = host_dir.join("privkey.pem");
        if cert.exists() && key.exists() {
            return Ok((
                cert.to_string_lossy().into_owned(),
                key.to_string_lossy().into_owned(),
            ));
        }
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

    /// Issue or retrieve a certificate for the given domain.
    pub async fn get_or_issue_cert(&self, domain: &str) -> anyhow::Result<(PathBuf, PathBuf)> {
        let client = self.acme_client.read().await;
        let client = client
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("ACME client not initialized (autorenew=false?)"))?;
        client.issue_cert(&[domain.to_string()]).await
    }

    /// Start background renewal task for given domains.
    pub async fn start_renewal(&self, domains: Vec<String>) {
        if !self.enabled {
            return;
        }
        if let Some(client) = self.acme_client.read().await.as_ref() {
            client.clone().start_background_renewal(domains);
        }
    }
}
