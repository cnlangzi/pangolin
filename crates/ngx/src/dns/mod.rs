//! DNS provider abstraction for ACME DNS-01 challenge.
//!
//! Implementations: Cloudflare, Aliyun DNS, Tencent DNSPod.

use base64::Engine;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use hickory_resolver::TokioAsyncResolver;
use pangolin_core::DnsProviderKind;

/// Trait for DNS providers that can create/delete TXT records.
#[async_trait]
#[allow(dead_code)]
pub trait DnsProvider: Send + Sync {
    /// Create a TXT record for DNS-01 challenge verification.
    async fn create_txt(&self, zone: &str, name: &str, value: &str, ttl: u32) -> Result<()>;

    /// Delete a TXT record by name.
    async fn delete_txt(&self, zone: &str, name: &str) -> Result<()>;

    /// Find the zone apex for a given FQDN.
    async fn find_zone(&self, fqdn: &str) -> Result<(String, String)>;
}

// ---------------------------------------------------------------------------
// Cloudflare
// ---------------------------------------------------------------------------

/// Cloudflare DNS provider using the REST API v4.
pub struct CloudflareDnsProvider {
    api_token: String,
    client: reqwest::Client,
}

impl CloudflareDnsProvider {
    pub fn new(api_token: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("Cloudflare client builder");
        Self { api_token, client }
    }

    async fn zone_by_name(&self, name: &str) -> Result<Option<(String, String)>> {
        let query = format!(
            "https://api.cloudflare.com/client/v4/zones?name={}",
            urlencoding::encode(name)
        );
        let resp = self
            .client
            .get(&query)
            .header("Authorization", format!("Bearer {}", self.api_token))
            .header("Content-Type", "application/json")
            .send()
            .await?;
        let body: serde_json::Value = resp.json().await?;
        let results = body
            .pointer("/result")
            .and_then(|r| r.as_array())
            .ok_or_else(|| anyhow::anyhow!("Cloudflare zones response malformed"))?;
        let best = results
            .iter()
            .filter_map(|z| {
                let name = z.pointer("/name")?.as_str()?.to_string();
                let id = z.pointer("/id")?.as_str()?.to_string();
                Some((name, id))
            })
            .max_by_key(|(n, _)| n.len());
        Ok(best)
    }

    #[allow(dead_code)]
    async fn record_id(&self, zone_id: &str, record_name: &str) -> Result<Option<String>> {
        let query = format!(
            "https://api.cloudflare.com/client/v4/zones/{}/dns_records?name={}",
            zone_id,
            urlencoding::encode(record_name)
        );
        let resp = self
            .client
            .get(&query)
            .header("Authorization", format!("Bearer {}", self.api_token))
            .header("Content-Type", "application/json")
            .send()
            .await?;
        let body: serde_json::Value = resp.json().await?;
        let results = body
            .pointer("/result")
            .and_then(|r| r.as_array())
            .ok_or_else(|| anyhow::anyhow!("Cloudflare dns_records response malformed"))?;
        let id = results
            .iter()
            .find(|r| {
                r.pointer("/name")
                    .and_then(|n| n.as_str())
                    .map(|n| n == record_name || n.ends_with(&format!(".{}", record_name)))
                    .unwrap_or(false)
            })
            .and_then(|r| r.pointer("/id"))
            .and_then(|id| id.as_str());
        Ok(id.map(String::from))
    }
}

#[async_trait]
impl DnsProvider for CloudflareDnsProvider {
    async fn create_txt(&self, _zone: &str, name: &str, value: &str, ttl: u32) -> Result<()> {
        let (zone_name, zone_id) = self
            .zone_by_name(name)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no Cloudflare zone found for {}", name))?;

        let payload = serde_json::json!({
            "type": "TXT",
            "name": name,
            "content": value,
            "ttl": ttl
        });

        let resp = self
            .client
            .post(format!(
                "https://api.cloudflare.com/client/v4/zones/{}/dns_records",
                zone_id
            ))
            .header("Authorization", format!("Bearer {}", self.api_token))
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await?;

        if !resp.status().is_success() {
            let body: serde_json::Value = resp.json().await?;
            let msg = body
                .pointer("/errors/0/message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            return Err(anyhow::anyhow!("Cloudflare create_txt failed: {}", msg));
        }
        log::info!(
            "Cloudflare TXT created: {} in zone {} ({})",
            name,
            zone_name,
            zone_id
        );
        Ok(())
    }

    async fn delete_txt(&self, _zone: &str, name: &str) -> Result<()> {
        let (zone_name, zone_id) = self
            .zone_by_name(name)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no Cloudflare zone found for {}", name))?;
        let _ = zone_name; // suppress unused warning (needed for tuple destructuring)

        if let Some(record_id) = self.record_id(&zone_id, name).await? {
            let resp = self
                .client
                .delete(format!(
                    "https://api.cloudflare.com/client/v4/zones/{}/dns_records/{}",
                    zone_id, record_id
                ))
                .header("Authorization", format!("Bearer {}", self.api_token))
                .send()
                .await?;
            if !resp.status().is_success() {
                log::warn!("Cloudflare delete_txt {} failed: {}", name, resp.status());
            }
        }
        Ok(())
    }

    async fn find_zone(&self, fqdn: &str) -> Result<(String, String)> {
        self.zone_by_name(fqdn)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no Cloudflare zone found for {}", fqdn))
    }
}

// ---------------------------------------------------------------------------
// Aliyun DNS
// ---------------------------------------------------------------------------

use std::collections::HashMap;

/// Aliyun DNS provider using the RPC API (Alidns 2015-01-09).
pub struct AliyunDnsProvider {
    access_key_id: String,
    access_key_secret: String,
    region: String,
    client: reqwest::Client,
}

impl AliyunDnsProvider {
    pub fn new(access_key_id: String, access_key_secret: String, region: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("Aliyun client builder");
        Self {
            access_key_id,
            access_key_secret,
            region: if region.is_empty() {
                "cn-hangzhou".into()
            } else {
                region
            },
            client,
        }
    }

    fn sign(&self, params: &mut HashMap<String, String>) {
        use hmac::{Hmac, Mac};
        type HmacSha1 = Hmac<sha1::Sha1>;

        let mut sorted: Vec<_> = params.iter().collect();
        sorted.sort_by_key(|p| p.0);

        let string_to_sign: String = sorted
            .iter()
            .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");

        let mut mac =
            HmacSha1::new_from_slice(self.access_key_secret.as_bytes()).expect("HMAC init");
        mac.update(string_to_sign.as_bytes());
        let signature =
            base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        params.insert("Signature".to_string(), signature);
    }

    async fn do_request(
        &self,
        action: &str,
        mut params: HashMap<String, String>,
    ) -> Result<serde_json::Value> {
        let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let nonce = uuid::Uuid::new_v4().to_string();

        params.insert("Format".to_string(), "JSON".to_string());
        params.insert("Version".to_string(), "2015-01-09".to_string());
        params.insert("AccessKeyId".to_string(), self.access_key_id.clone());
        params.insert("SignatureMethod".to_string(), "HMAC-SHA1".to_string());
        params.insert("SignatureVersion".to_string(), "1.0".to_string());
        params.insert("SignatureNonce".to_string(), nonce);
        params.insert("Timestamp".to_string(), timestamp);
        params.insert("RegionId".to_string(), self.region.clone());
        params.insert("Action".to_string(), action.to_string());

        self.sign(&mut params);

        let query: String = params
            .iter()
            .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");

        let url = format!("https://alidns.aliyuncs.com/?{}", query);

        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .send()
            .await?;

        let body: serde_json::Value = resp.json().await?;
        if let Some(code) = body.pointer("/Code") {
            let msg = body
                .pointer("/Message")
                .and_then(|m| m.as_str())
                .unwrap_or("?");
            return Err(anyhow::anyhow!(
                "Aliyun {} failed: {} — {}",
                action,
                code.as_str().unwrap_or("?"),
                msg
            ));
        }
        Ok(body)
    }
}

#[async_trait]
impl DnsProvider for AliyunDnsProvider {
    async fn create_txt(&self, _zone: &str, name: &str, value: &str, ttl: u32) -> Result<()> {
        let mut params = HashMap::new();
        params.insert("DomainName".to_string(), name.to_string());
        params.insert("RR".to_string(), "_acme-challenge".to_string());
        params.insert("Type".to_string(), "TXT".to_string());
        params.insert("Value".to_string(), value.to_string());
        params.insert("TTL".to_string(), ttl.to_string());

        self.do_request("AddDomainRecord", params).await?;
        log::info!("Aliyun TXT created: {} for {}", value, name);
        Ok(())
    }

    async fn delete_txt(&self, _zone: &str, name: &str) -> Result<()> {
        let mut params = HashMap::new();
        params.insert("DomainName".to_string(), name.to_string());
        params.insert("RRKeyWord".to_string(), "_acme-challenge".to_string());
        params.insert("Type".to_string(), "TXT".to_string());

        let resp = self.do_request("DescribeDomainRecords", params).await?;
        let records = resp
            .pointer("/DomainRecords/Record")
            .and_then(|r| r.as_array())
            .ok_or_else(|| anyhow::anyhow!("Aliyun DescribeDomainRecords response malformed"))?;

        for record in records {
            let record_id = record
                .pointer("/RecordId")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing RecordId"))?;
            let mut dp = HashMap::new();
            dp.insert("RecordId".to_string(), record_id.to_string());
            self.do_request("DeleteDomainRecord", dp).await?;
        }
        Ok(())
    }

    async fn find_zone(&self, fqdn: &str) -> Result<(String, String)> {
        let parts: Vec<_> = fqdn.split('.').collect();
        for i in 0..parts.len() {
            let candidate = parts[i..].join(".");
            let mut params = HashMap::new();
            params.insert("DomainName".to_string(), candidate.clone());
            if let Ok(resp) = self.do_request("DescribeDomainRecords", params).await {
                if resp.pointer("/DomainRecords/Record").is_some() {
                    return Ok((candidate.clone(), candidate));
                }
            }
        }
        Err(anyhow::anyhow!("no Aliyun zone found for {}", fqdn))
    }
}

// ---------------------------------------------------------------------------
// Tencent DNSPod
// ---------------------------------------------------------------------------

/// Tencent DNSPod provider using REST API v3.
pub struct TencentDnsProvider {
    secret_id: String,
    secret_key: String,
    client: reqwest::Client,
}

impl TencentDnsProvider {
    pub fn new(secret_id: String, secret_key: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("Tencent client builder");
        Self {
            secret_id,
            secret_key,
            client,
        }
    }

    fn sign(&self, params: &mut HashMap<String, String>) -> String {
        use hmac::{Hmac, Mac};
        type HmacSha256 = Hmac<sha2::Sha256>;

        let mut sorted: Vec<_> = params.iter().collect();
        sorted.sort_by_key(|p| p.0);

        let string_to_sign = sorted
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join("&");

        let body_hash = "da39a3ee5e6b4b0d3255bfef95601890afd80709".to_string();
        let data = format!("{}\n{}", string_to_sign, body_hash);

        let mut mac = HmacSha256::new_from_slice(self.secret_key.as_bytes()).expect("HMAC init");
        mac.update(data.as_bytes());
        base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
    }

    async fn do_request(
        &self,
        action: &str,
        mut params: HashMap<String, String>,
    ) -> Result<serde_json::Value> {
        use std::time::SystemTime;

        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();
        let nonce = uuid::Uuid::new_v4().to_string();

        params.insert("SecretId".to_string(), self.secret_id.clone());
        params.insert("Timestamp".to_string(), timestamp);
        params.insert("Nonce".to_string(), nonce);
        params.insert("Action".to_string(), action.to_string());
        params.insert("Version".to_string(), "2021-03-23".to_string());

        let signature = self.sign(&mut params);

        let query: String = params
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join("&");

        let url = format!("https://dnspod.tencentcloudapi.com/?{}", query);

        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Authorization", format!("TC3-HMAC-SHA256 {}", signature))
            .send()
            .await?;

        let body: serde_json::Value = resp.json().await?;
        if let Some(code) = body.pointer("/Response/Error/Code") {
            let msg = body
                .pointer("/Response/Error/Message")
                .and_then(|m| m.as_str())
                .unwrap_or("?");
            return Err(anyhow::anyhow!(
                "Tencent {} failed: {} — {}",
                action,
                code.as_str().unwrap_or("?"),
                msg
            ));
        }
        Ok(body)
    }
}

#[async_trait]
impl DnsProvider for TencentDnsProvider {
    async fn create_txt(&self, _zone: &str, name: &str, value: &str, ttl: u32) -> Result<()> {
        let mut params = HashMap::new();
        params.insert("Domain".to_string(), name.to_string());
        params.insert("SubDomain".to_string(), "_acme-challenge".to_string());
        params.insert("RecordType".to_string(), "TXT".to_string());
        params.insert("Value".to_string(), value.to_string());
        params.insert("TTL".to_string(), ttl.to_string());

        self.do_request("CreateRecord", params).await?;
        log::info!("Tencent TXT created: {} for {}", value, name);
        Ok(())
    }

    async fn delete_txt(&self, _zone: &str, name: &str) -> Result<()> {
        let mut params = HashMap::new();
        params.insert("Domain".to_string(), name.to_string());
        params.insert("SubDomain".to_string(), "_acme-challenge".to_string());
        params.insert("RecordType".to_string(), "TXT".to_string());

        let resp = self.do_request("DescribeRecords", params).await?;
        let records = resp
            .pointer("/Response/RecordList")
            .and_then(|r| r.as_array());

        if let Some(recs) = records {
            for record in recs {
                let record_id = record
                    .pointer("/RecordId")
                    .and_then(|v| v.as_i64())
                    .map(|v| v.to_string());
                if let Some(rid) = record_id {
                    let mut dp = HashMap::new();
                    dp.insert("RecordId".to_string(), rid);
                    self.do_request("DeleteRecord", dp).await?;
                }
            }
        }
        Ok(())
    }

    async fn find_zone(&self, fqdn: &str) -> Result<(String, String)> {
        // Walk up the FQDN one label at a time, asking the DNSPod API
        // "does this candidate exist as a zone on the account?" The
        // first hit wins — for `frtpilot.yaitoo.cn` we try
        //   1. frtpilot.yaitoo.cn  (operator zone? unlikely)
        //   2. yaitoo.cn           (typical apex zone)
        //   3. cn                  (TLD, will always miss)
        //
        // The previous implementation used `if let Ok(resp)` and
        // silently dropped EVERY error — so a SignatureFailure or
        // network outage produced the same "no Tencent zone found"
        // as a legitimate miss. Operators couldn't tell if their
        // credentials were wrong, their zone wasn't on this account,
        // or the API was down.
        //
        // The new contract:
        //   - `DomainRecordNotExist` / "domain not found" style
        //     errors are non-fatal: this candidate isn't on the
        //     account, try the parent label.
        //   - Any other error (signature, auth, rate limit, 5xx,
        //     network) is fatal — bubbled up immediately so the
        //     operator sees the real cause instead of a misleading
        //     "no zone found".
        //   - Every probe is logged so a `tail -f` reveals the walk.
        let parts: Vec<_> = fqdn.split('.').collect();
        // First label cannot be a zone on its own (Tencent doesn't
        // accept TLD-level zones), skip i=parts.len()-1.
        let upper = parts.len().saturating_sub(1);
        let mut last_miss: Option<String> = None;
        for i in 0..upper {
            let candidate = parts[i..].join(".");
            let mut params = HashMap::new();
            params.insert("Domain".to_string(), candidate.clone());
            log::debug!("Tencent find_zone: probing {}", candidate);
            match self.do_request("DescribeRecords", params).await {
                Ok(resp) => {
                    if resp.pointer("/Response/RecordList").is_some() {
                        log::info!(
                            "Tencent find_zone: matched {} for fqdn {}",
                            candidate,
                            fqdn
                        );
                        return Ok((candidate.clone(), candidate));
                    }
                    // 200 OK but no RecordList — odd, treat as miss.
                    last_miss = Some(format!("{}: empty response", candidate));
                }
                Err(e) => {
                    let msg = e.to_string();
                    if is_zone_not_on_account(&msg) {
                        // Expected miss — continue walking.
                        log::debug!(
                            "Tencent find_zone: {} not on account ({}), trying parent",
                            candidate,
                            msg
                        );
                        last_miss = Some(format!("{}: {}", candidate, msg));
                        continue;
                    }
                    // Real error (auth, signature, rate limit, 5xx,
                    // network). Bail with the underlying cause so the
                    // operator sees what's actually wrong.
                    return Err(anyhow::anyhow!(
                        "Tencent find_zone probe for {} failed: {}. \
                         Common causes: SecretId/SecretKey wrong, \
                         credential lacks DNSPod DescribeRecords permission, \
                         or DNSPod API outage. Check the Tencent console.",
                        candidate,
                        msg
                    ));
                }
            }
        }
        Err(anyhow::anyhow!(
            "no Tencent zone found for {} after probing {} candidate(s); \
             last miss: {}. Verify the apex zone is registered on this \
             Tencent account.",
            fqdn,
            upper,
            last_miss.unwrap_or_else(|| "n/a".into())
        ))
    }
}

/// Tencent DNSPod returns specific error codes when a domain isn't
/// registered on the account, vs auth/system errors. Treat the former
/// as "try the next candidate"; everything else is a real failure
/// that the operator needs to see.
///
/// Codes / phrases observed:
///   - `DomainRecordNotExist` (modern)
///   - `InvalidParameter.DomainInvalid` (when probing a non-zone label)
///   - `ResourceNotFound.NoDataOfDomain`
///   - "domain not found" / "no permission" with a not-on-account hint
///
/// Anything else (signature failure, rate limit, network) returns
/// false so the caller bubbles the error.
fn is_zone_not_on_account(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    lower.contains("domainrecordnotexist")
        || lower.contains("domaininvalid")
        || lower.contains("nodataofdomain")
        || lower.contains("domain not found")
        || lower.contains("not exist")
}

// ---------------------------------------------------------------------------
// TXT propagation check via hickory-resolver
// ---------------------------------------------------------------------------

/// Poll for TXT record visibility using hickory-resolver querying 8.8.8.8/1.1.1.1.
pub async fn wait_for_txt_propagation(
    fqdn: &str,
    expected_value: &str,
    timeout_secs: u64,
    poll_interval_secs: u64,
) -> Result<bool> {
    let resolver = TokioAsyncResolver::tokio(
        hickory_resolver::config::ResolverConfig::cloudflare(),
        hickory_resolver::config::ResolverOpts::default(),
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);

    let fqdn_clean = fqdn.trim_start_matches('_').trim_start_matches('.');

    loop {
        match resolver.txt_lookup(fqdn_clean).await {
            Ok(lookup) => {
                for rdata in lookup.iter() {
                    for txt_slice in rdata.txt_data() {
                        if let Ok(txt_str) = std::str::from_utf8(txt_slice) {
                            if txt_str.contains(expected_value) {
                                log::info!(
                                    "DNS-01 TXT record found for {} after propagation",
                                    fqdn_clean
                                );
                                return Ok(true);
                            }
                        }
                    }
                }
            }
            Err(e) => {
                log::debug!("DNS lookup for {} not yet visible: {}", fqdn_clean, e);
            }
        }

        if std::time::Instant::now() >= deadline {
            log::warn!(
                "DNS-01 propagation timeout for {} after {}s",
                fqdn_clean,
                timeout_secs
            );
            return Ok(false);
        }

        tokio::time::sleep(std::time::Duration::from_secs(poll_interval_secs)).await;
    }
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/// Build a DNS provider from a kind + plaintext JSON config blob (read from
/// the `dns_providers` table). Returns Err on unknown kind or unparseable
/// config. The caller is responsible for surfacing the error to the admin UI
/// (e.g. via the events log).
pub fn from_kind_config(kind: DnsProviderKind, config_json: &str) -> Result<Arc<dyn DnsProvider>> {
    let v: serde_json::Value = serde_json::from_str(config_json)
        .map_err(|e| anyhow!("invalid dns provider config JSON: {e}"))?;
    match kind {
        DnsProviderKind::Cloudflare => {
            let token = v
                .get("api_token")
                .and_then(|x| x.as_str())
                .ok_or_else(|| anyhow!("cloudflare config missing 'api_token'"))?
                .to_string();
            if token.is_empty() {
                return Err(anyhow!("cloudflare 'api_token' is empty"));
            }
            Ok(Arc::new(CloudflareDnsProvider::new(token)))
        }
        DnsProviderKind::Aliyun => {
            let ak_id = v
                .get("access_key_id")
                .and_then(|x| x.as_str())
                .ok_or_else(|| anyhow!("aliyun config missing 'access_key_id'"))?
                .to_string();
            let ak_secret = v
                .get("access_key_secret")
                .and_then(|x| x.as_str())
                .ok_or_else(|| anyhow!("aliyun config missing 'access_key_secret'"))?
                .to_string();
            let region = v
                .get("region")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            if ak_id.is_empty() || ak_secret.is_empty() {
                return Err(anyhow!(
                    "aliyun access_key_id and access_key_secret must be non-empty"
                ));
            }
            Ok(Arc::new(AliyunDnsProvider::new(ak_id, ak_secret, region)))
        }
        DnsProviderKind::Tencent => {
            let secret_id = v
                .get("secret_id")
                .and_then(|x| x.as_str())
                .ok_or_else(|| anyhow!("tencent config missing 'secret_id'"))?
                .to_string();
            let secret_key = v
                .get("secret_key")
                .and_then(|x| x.as_str())
                .ok_or_else(|| anyhow!("tencent config missing 'secret_key'"))?
                .to_string();
            if secret_id.is_empty() || secret_key.is_empty() {
                return Err(anyhow!(
                    "tencent secret_id and secret_key must be non-empty"
                ));
            }
            Ok(Arc::new(TencentDnsProvider::new(secret_id, secret_key)))
        }
    }
}


#[cfg(test)]
mod tests {
    use super::is_zone_not_on_account;

    #[test]
    fn classifier_treats_missing_domain_as_skip() {
        // Known Tencent error codes / phrases that mean "this label
        // is not a zone on the account" — caller should walk to the
        // parent label, not bail.
        for msg in [
            "Tencent DescribeRecords failed: DomainRecordNotExist — domain not found",
            "InvalidParameter.DomainInvalid — bad input",
            "ResourceNotFound.NoDataOfDomain",
            "domain not found",
            "the resource does not exist",
        ] {
            assert!(is_zone_not_on_account(msg), "should be skippable: {msg}");
        }
    }

    #[test]
    fn classifier_keeps_real_errors_as_fatal() {
        // Auth / signature / rate-limit / 5xx — caller must bubble
        // these so the operator sees the real cause instead of a
        // misleading "no zone found".
        for msg in [
            "Tencent DescribeRecords failed: AuthFailure.SignatureFailure — wrong secret",
            "RequestLimitExceeded",
            "InternalError",
            "connection reset by peer",
            "503 Service Unavailable",
        ] {
            assert!(!is_zone_not_on_account(msg), "must be fatal: {msg}");
        }
    }
}
