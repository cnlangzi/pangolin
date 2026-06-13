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

/// Flatten an error and its `source()` chain into a single
/// human-readable string. Used by the Tencent / Aliyun / Cloudflare
/// transports so the operator sees the real underlying cause (DNS,
/// TLS, TCP refused, timeout) instead of reqwest's outer
/// `"error sending request for url (...)"` wrapper.
///
/// **Every layer's `Display` output is run through `scrub_url_query`
/// before being included.** reqwest's `Display` impl carries the full
/// request URL, and the Tencent API puts `SecretId` in the query
/// string — without scrubbing, a single `log::warn!` of the error
/// chain leaks the credential to admin logs and the dashboard
/// activity panel. The scrubber strips everything after `?` from any
/// URL appearing in the message so only host + path remain.
///
/// Stops after a small ceiling so a malicious or pathological cause
/// chain can't produce unbounded log output.
fn chain_message<E: std::error::Error + ?Sized>(e: &E) -> String {
    let mut parts: Vec<String> = vec![scrub_url_query(&e.to_string())];
    let mut src = e.source();
    let mut depth = 0;
    while let Some(s) = src {
        if depth >= 6 {
            parts.push("…".to_string());
            break;
        }
        parts.push(scrub_url_query(&s.to_string()));
        src = s.source();
        depth += 1;
    }
    // Deduplicate — reqwest sometimes nests the same message twice.
    parts.dedup();
    parts.join(" → ")
}

/// Strip `?query` from any URL appearing in a message. Used because
/// Tencent's API takes `SecretId` in the query string (cleartext) and
/// reqwest's `Display` impl naïvely includes the full URL in its
/// error message — so anything that propagates a reqwest error
/// without scrubbing leaks the credential to logs.
///
/// Conservative — only strips between `?` and the next whitespace /
/// `)` / end-of-string, so non-URL text containing `?` (e.g. an
/// English sentence) is unaffected as long as the suffix doesn't
/// look like a URL path.
///
/// Replaces the stripped query with `?…` so the message reads
/// naturally and the reader knows redaction happened.
fn scrub_url_query(msg: &str) -> String {
    // Walk the string finding `://...?...<terminator>` runs and
    // substituting the query span with `?…`. Implementation uses
    // `find`/`split_at` on string slices so multi-byte UTF-8
    // (e.g. the `→` chain separator) round-trips unchanged.
    let mut remaining = msg;
    let mut out = String::with_capacity(msg.len());
    while !remaining.is_empty() {
        match remaining.find("://") {
            None => {
                out.push_str(remaining);
                break;
            }
            Some(scheme_idx) => {
                // Copy everything up to and including `://`.
                let scheme_end = scheme_idx + 3;
                out.push_str(&remaining[..scheme_end]);
                let rest = &remaining[scheme_end..];
                // Find the URL-end terminator (space/paren/newline/tab)
                // and the optional `?` inside the run.
                let terminator = rest.find([' ', ')', '\n', '\t']).unwrap_or(rest.len());
                let run = &rest[..terminator];
                match run.find('?') {
                    None => {
                        // No query — copy the URL as-is.
                        out.push_str(run);
                    }
                    Some(q) => {
                        // Copy host+path+`?`, then redact the query.
                        out.push_str(&run[..=q]);
                        out.push('…');
                    }
                }
                remaining = &rest[terminator..];
            }
        }
    }
    out
}

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

    /// Live credential / connectivity probe used by `POST /dns/test`.
    ///
    /// The default impl tries `find_zone` against a domain that is
    /// statistically unlikely to be on the account (`__pangolin-check__.local`):
    /// a successful API call that returns "not on account" proves the
    /// credentials work, while an auth/network/permission failure
    /// surfaces the real cause to the admin UI. Providers can override
    /// for cheaper / more targeted checks.
    async fn probe(&self) -> Result<String> {
        match self.find_zone("__pangolin-check__.local.invalid").await {
            Ok((z, _)) => Ok(format!(
                "credentials work; probe matched zone {} (unexpected — \
                 check the probe domain isn't actually registered)",
                z
            )),
            Err(e) => {
                let msg = e.to_string();
                // "no zone found" is the expected probe outcome: the
                // API responded, parsed our response, said "not on
                // account". That means credentials + network are OK.
                if msg.contains("no ") && msg.contains("zone found") {
                    Ok("credentials and network OK (probe returned 'no zone' as expected)".into())
                } else {
                    Err(e)
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Cloudflare
// ---------------------------------------------------------------------------

/// Cloudflare DNS provider using the REST API v4.
pub struct CloudflareDnsProvider {
    api_token: String,
    client: reqwest::Client,
}

/// Build a reqwest client for DNS provider API calls (Cloudflare,
/// DNSPod, Aliyun).
///
/// **TLS verification is intentionally DISABLED.** The workspace uses
/// `rustls-tls-manual-roots` (no CA bundle compiled in, no OS trust
/// store reader) so the default reqwest client has zero trusted
/// roots and rejects every public CA with `UnknownIssuer`. Loading
/// roots would mean adding `webpki-roots` (bundled Mozilla CA) or
/// `rustls-native-certs` (OS trust store) — both bring deployment
/// complexity that the user explicitly declined: pangolin is
/// deployed to a trusted server environment, the DNS provider hosts
/// (`dnspod.tencentcloudapi.com` / `api.cloudflare.com` /
/// `alidns.aliyuncs.com`) are themselves trusted infrastructure,
/// and the operator owns both sides.
///
/// A one-line WARN at construction makes the choice visible in the
/// startup log so the next person reading it doesn't wonder why
/// MITM doesn't trip an alert.
fn build_dns_client(provider: &str) -> reqwest::Client {
    log::warn!(
        "DNS[{}] reqwest client: timeout=15s, TLS cert verification DISABLED \
         (trusted environment — see crates/ngx/src/dns/mod.rs::build_dns_client)",
        provider
    );
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap_or_else(|e| panic!("{} client builder: {}", provider, e))
}

impl CloudflareDnsProvider {
    pub fn new(api_token: String) -> Self {
        Self {
            api_token,
            client: build_dns_client("Cloudflare"),
        }
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
        Self {
            access_key_id,
            access_key_secret,
            region: if region.is_empty() {
                "cn-hangzhou".into()
            } else {
                region
            },
            client: build_dns_client("Aliyun"),
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
        Self {
            secret_id,
            secret_key,
            client: build_dns_client("DNSPod"),
        }
    }

    /// TC3-HMAC-SHA256 signing per Tencent Cloud API 3.0 spec.
    /// See: https://cloud.tencent.com/document/api/1427/56188
    ///
    /// Returns the `Signature` hex string that goes into the
    /// `Authorization: TC3-HMAC-SHA256 ... Signature=<this>` header.
    ///
    /// **Body and headers, NOT query string.** The previous Frankenstein
    /// impl signed sigv1-style (`key=value&...` joined, base64 output,
    /// empty-body hash hard-coded) but advertised TC3 in the
    /// Authorization header. Tencent dutifully reported "MissingParameter
    /// X-TC-Action" because the action lived in the URL query instead of
    /// the required header. Operators saw "find_zone failed" with no clue
    /// it was our signer.
    fn sign_tc3(&self, date: &str, timestamp: i64, action_lower: &str, body_json: &str) -> String {
        use hmac::{Hmac, Mac};
        use sha2::{Digest, Sha256};
        type HmacSha256 = Hmac<Sha256>;

        let sha256_hex = |bytes: &[u8]| -> String {
            let mut h = Sha256::new();
            h.update(bytes);
            let digest = h.finalize();
            let mut s = String::with_capacity(64);
            for b in digest {
                use std::fmt::Write;
                let _ = write!(&mut s, "{:02x}", b);
            }
            s
        };

        let payload_hash = sha256_hex(body_json.as_bytes());

        // CanonicalRequest — order and case matter exactly.
        let canonical_req = format!(
            "POST\n\
             /\n\
             \n\
             content-type:application/json; charset=utf-8\n\
             host:{HOST}\n\
             x-tc-action:{action}\n\
             \n\
             content-type;host;x-tc-action\n\
             {payload_hash}",
            HOST = "dnspod.tencentcloudapi.com",
            action = action_lower,
        );
        let credential_scope = format!("{date}/dnspod/tc3_request");
        let canonical_hash = sha256_hex(canonical_req.as_bytes());
        let string_to_sign =
            format!("TC3-HMAC-SHA256\n{timestamp}\n{credential_scope}\n{canonical_hash}");

        // Derive signing key: SK4 chain (per Tencent spec).
        let mut mac = HmacSha256::new_from_slice(format!("TC3{}", self.secret_key).as_bytes())
            .expect("HMAC init");
        mac.update(date.as_bytes());
        let secret_date = mac.finalize().into_bytes();

        let mut mac = HmacSha256::new_from_slice(&secret_date).expect("HMAC init");
        mac.update(b"dnspod");
        let secret_service = mac.finalize().into_bytes();

        let mut mac = HmacSha256::new_from_slice(&secret_service).expect("HMAC init");
        mac.update(b"tc3_request");
        let secret_signing = mac.finalize().into_bytes();

        let mut mac = HmacSha256::new_from_slice(&secret_signing).expect("HMAC init");
        mac.update(string_to_sign.as_bytes());
        let sig = mac.finalize().into_bytes();
        let mut hex = String::with_capacity(64);
        for b in sig {
            use std::fmt::Write;
            let _ = write!(&mut hex, "{:02x}", b);
        }
        hex
    }

    async fn do_request(
        &self,
        action: &str,
        payload: HashMap<String, String>,
    ) -> Result<serde_json::Value> {
        use std::time::SystemTime;
        const HOST: &str = "dnspod.tencentcloudapi.com";
        const SERVICE: &str = "dnspod";
        const VERSION: &str = "2021-03-23";

        // ── SECURITY: any error message that flows from here MUST
        // never include the SecretId in cleartext. Keep a 4-char
        // tail (helps operators match it against their console)
        // and a constant `safe_url` (no query string at all under
        // TC3 — payload is in body, headers carry the metadata).
        let safe_url = format!("https://{HOST}/");
        let secret_id_tail: String = self.secret_id.chars().rev().take(4).collect();
        let secret_id_tail: String = secret_id_tail.chars().rev().collect();

        // Build JSON body from the payload HashMap. Order doesn't
        // matter for Tencent; the signature is over the exact
        // string bytes we send.
        let body_value = serde_json::Value::Object(
            payload
                .into_iter()
                .map(|(k, v)| (k, serde_json::Value::String(v)))
                .collect(),
        );
        let body_json = serde_json::to_string(&body_value).unwrap_or_else(|_| "{}".to_string());

        let now_secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let date = chrono::DateTime::<chrono::Utc>::from_timestamp(now_secs, 0)
            .ok_or_else(|| anyhow::anyhow!("invalid timestamp"))?
            .format("%Y-%m-%d")
            .to_string();
        let action_lower = action.to_ascii_lowercase();
        let signature = self.sign_tc3(&date, now_secs, &action_lower, &body_json);
        let auth = format!(
            "TC3-HMAC-SHA256 Credential={sid}/{date}/{SERVICE}/tc3_request, \
             SignedHeaders=content-type;host;x-tc-action, Signature={signature}",
            sid = self.secret_id,
        );

        let resp = match self
            .client
            .post(&safe_url)
            .header("Content-Type", "application/json; charset=utf-8")
            .header("Host", HOST)
            .header("X-TC-Action", action)
            .header("X-TC-Timestamp", now_secs.to_string())
            .header("X-TC-Version", VERSION)
            .header("Authorization", auth)
            .body(body_json)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let chain = chain_message(&e);
                return Err(anyhow::anyhow!(
                    "Tencent {} HTTP failure ({}, SecretId=…{}): {}",
                    action,
                    safe_url,
                    secret_id_tail,
                    chain
                ));
            }
        };
        let body: serde_json::Value = match resp.json().await {
            Ok(b) => b,
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "Tencent {} body parse failure ({}, SecretId=…{}): {}",
                    action,
                    safe_url,
                    secret_id_tail,
                    chain_message(&e)
                ));
            }
        };
        if let Some(code) = body.pointer("/Response/Error/Code") {
            let msg = body
                .pointer("/Response/Error/Message")
                .and_then(|m| m.as_str())
                .unwrap_or("?");
            return Err(anyhow::anyhow!(
                "Tencent {} failed (SecretId=…{}): {} — {}",
                action,
                secret_id_tail,
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
                        log::info!("Tencent find_zone: matched {} for fqdn {}", candidate, fqdn);
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
    use super::{chain_message, is_zone_not_on_account, scrub_url_query};

    #[test]
    fn scrub_url_query_strips_query_string() {
        // Tencent's SecretId travels in URL query — must not survive
        // into log/event messages. Real-world reqwest error string
        // (verbatim from the user's bug report, credential redacted).
        let in_ = "error sending request for url \
                   (https://dnspod.tencentcloudapi.com/?Action=Describe&SecretId=AKID123&Domain=foo) \
                   → invalid peer certificate";
        let out = scrub_url_query(in_);
        assert!(!out.contains("SecretId"), "must redact: {out}");
        assert!(!out.contains("AKID"), "must redact: {out}");
        assert!(out.contains("dnspod.tencentcloudapi.com/?…"), "{out}");
        // Preserve everything outside the URL — multibyte arrow
        // included.
        assert!(out.contains(" → invalid peer certificate"), "{out}");
    }

    #[test]
    fn scrub_url_query_passes_through_non_url_text() {
        // English sentences with `?` are not URLs; do not redact.
        let in_ = "what is this? — a question";
        let out = scrub_url_query(in_);
        assert_eq!(out, in_);
    }

    #[test]
    fn scrub_url_query_handles_url_without_query() {
        let in_ = "GET https://example.com/path/here failed";
        let out = scrub_url_query(in_);
        assert_eq!(out, in_, "URL without `?` is unchanged");
    }

    #[test]
    fn scrub_url_query_handles_multiple_urls() {
        let in_ = "first https://a.com/?k=1 then https://b.com/?j=2 done";
        let out = scrub_url_query(in_);
        assert!(out.contains("https://a.com/?…"), "{out}");
        assert!(out.contains("https://b.com/?…"), "{out}");
        assert!(!out.contains("k=1"), "{out}");
        assert!(!out.contains("j=2"), "{out}");
    }

    #[test]
    fn chain_message_flattens_source_chain() {
        // Simulate a reqwest-style nested error: outer "error sending
        // request" wraps an inner DNS / TLS / IO error. The flatten
        // helper must surface both so operators see the real cause
        // instead of the outer wrapper alone.
        use std::error::Error;
        use std::fmt;

        #[derive(Debug)]
        struct Inner;
        impl fmt::Display for Inner {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("connection refused")
            }
        }
        impl Error for Inner {}

        #[derive(Debug)]
        struct Outer;
        impl fmt::Display for Outer {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("error sending request")
            }
        }
        impl Error for Outer {
            fn source(&self) -> Option<&(dyn Error + 'static)> {
                Some(&Inner)
            }
        }

        let msg = chain_message(&Outer);
        assert!(msg.contains("error sending request"), "{msg}");
        assert!(msg.contains("connection refused"), "{msg}");
        assert!(msg.contains(" → "), "{msg}");
    }

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
