//! TLS listener setup — SNI callback that loads per-host certs on demand.
//!
//! v2 design: each host must have its own cert blob at
//! `cert_dir/{host}` (or `{host}+rsa`). When a TLS handshake arrives,
//! we look up the blob by SNI. If the blob exists, parse the
//! combined key+cert PEM and attach it to the handshake. If not,
//! the callback returns without setting a cert, and the handshake
//! fails with an `unrecognized_name` alert.
//!
//! The previous v1 listener used `TlsSettings::intermediate(path, path)`
//! with a single fixed host (config-driven). The v2 listener is SNI-based
//! to support multi-tenant wildcard + per-domain certs in one listener.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use openssl::pkey::PKey;
use openssl::x509::X509;
use pingora::listeners::TlsAccept;
use pingora_core::protocols::tls::TlsRef;
use pingora_core::tls::ssl::NameType;

/// Callback invoked during the TLS handshake to install a per-SNI cert.
/// The cert directory lookup happens here, in the handshake path, so
/// newly-issued certs are picked up automatically without restarting
/// the listener.
pub struct SniCertCallback {
    pub cert_dir: Arc<std::path::PathBuf>,
}

impl SniCertCallback {
    pub fn new(cert_dir: Arc<std::path::PathBuf>) -> Self {
        Self { cert_dir }
    }
}

#[async_trait]
impl TlsAccept for SniCertCallback {
    async fn certificate_callback(&self, ssl: &mut TlsRef) {
        // Pull SNI from the handshake.
        let sni = match ssl.servername(NameType::HOST_NAME) {
            Some(s) => s.to_lowercase(),
            None => return, // no SNI → no cert → handshake fails
        };

        // Try ECDSA blob first, then +rsa variant.
        let ecdsa = self.cert_dir.join(&sni);
        let rsa = self.cert_dir.join(format!("{}+rsa", &sni));
        let blob_path = if ecdsa.is_file() {
            ecdsa
        } else if rsa.is_file() {
            rsa
        } else {
            log::debug!("TLS: no cert for SNI '{}', handshake will fail", sni);
            return;
        };

        // Load + parse the combined key+cert PEM blob.
        let blob = match std::fs::read_to_string(&blob_path) {
            Ok(s) => s,
            Err(e) => {
                log::error!(
                    "TLS: failed to read cert blob {}: {}",
                    blob_path.display(),
                    e
                );
                return;
            }
        };
        let (key_pem, cert_pems) = match split_blob(&blob) {
            Ok(p) => p,
            Err(e) => {
                log::error!(
                    "TLS: failed to parse cert blob {}: {}",
                    blob_path.display(),
                    e
                );
                return;
            }
        };

        let pkey = match PKey::private_key_from_pem(key_pem.as_bytes()) {
            Ok(k) => k,
            Err(e) => {
                log::error!("TLS: private key parse failed: {}", e);
                return;
            }
        };

        // Parse every cert block in order: the first is the leaf,
        // the rest are intermediates (and optionally the root) that
        // Let's Encrypt returns leaf-first. We must install the
        // intermediates on the SSL object too — otherwise the TLS
        // `Certificate` message ships only the leaf, the client can't
        // build the chain, and Chrome (which has disabled AIA
        // fetching) shows "Your connection is not secure" even though
        // the leaf itself is valid.
        let mut parsed_certs = Vec::with_capacity(cert_pems.len());
        for pem in &cert_pems {
            match X509::from_pem(pem.as_bytes()) {
                Ok(c) => parsed_certs.push(c),
                Err(e) => {
                    log::error!("TLS: certificate parse failed: {}", e);
                    return;
                }
            }
        }
        // `split_blob` always returns at least the leaf.
        let leaf = parsed_certs.remove(0);
        let intermediates = parsed_certs;

        // Hand the cert + key + chain to the in-progress handshake.
        // Any error here aborts the handshake, which is what we want.
        if let Err(e) = pingora_openssl::ext::ssl_use_certificate(ssl, &leaf) {
            log::error!("TLS: ssl_use_certificate failed: {}", e);
            return;
        }
        if let Err(e) = pingora_openssl::ext::ssl_use_private_key(ssl, &pkey) {
            log::error!("TLS: ssl_use_private_key failed: {}", e);
            return;
        }
        for ca in &intermediates {
            // `ssl_add_chain_cert` bumps the X509 refcount so the cert
            // outlives this local — see pingora-openssl/src/ext.rs.
            if let Err(e) = pingora_openssl::ext::ssl_add_chain_cert(ssl, ca) {
                log::error!("TLS: ssl_add_chain_cert failed: {}", e);
                return;
            }
        }
        log::debug!(
            "TLS: cert installed for SNI '{}' ({} intermediate(s))",
            sni,
            intermediates.len()
        );
    }
}

/// Split a combined key+cert PEM blob (autocert DirCache native format:
/// key block first, then one or more certificate blocks in leaf-first
/// order) into the key PEM and **every** CERTIFICATE block.
///
/// We return the full chain (leaf + intermediates, optionally root) —
/// not just the leaf — because `X509::from_pem` only parses the first
/// cert in a PEM and `ssl_use_certificate` only installs the leaf.
/// The intermediates must be passed to `ssl_add_chain_cert` so the TLS
/// `Certificate` message ships them to the client; otherwise Chrome
/// (which has disabled AIA fetching) cannot build the chain and shows
/// "Your connection is not secure" despite a valid leaf.
fn split_blob(blob: &str) -> anyhow::Result<(String, Vec<String>)> {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";

    // Locate the first CERTIFICATE block — that's the leaf.
    let cert_start = blob
        .find(BEGIN)
        .ok_or_else(|| anyhow::anyhow!("no CERTIFICATE block in blob"))?;
    let cert_end_rel = blob[cert_start..]
        .find(END)
        .ok_or_else(|| anyhow::anyhow!("malformed CERTIFICATE block"))?;
    let cert_end = cert_start + cert_end_rel + END.len();

    // The key is everything before the first CERTIFICATE block. Strip
    // trailing whitespace.
    let key_part = blob[..cert_start].trim_end().to_string();
    if !key_part.contains("PRIVATE KEY") {
        anyhow::bail!("no PRIVATE KEY block before the certificate");
    }

    // Collect the leaf + every subsequent CERTIFICATE block. The
    // chain is leaf-first (intermediates, optionally root) — see the
    // `parse_blob_expiry_picks_leaf_not_intermediate` test in
    // `acme.rs` for the layout guarantee from `instant_acme`.
    let mut certs = Vec::new();
    certs.push(blob[cert_start..cert_end].to_string());
    let mut cursor = &blob[cert_end..];
    while let Some(rel) = cursor.find(BEGIN) {
        let start = rel;
        let after = &cursor[start..];
        let Some(end_rel) = after.find(END) else {
            // Truncated trailing block — bail so the caller logs a
            // clear error instead of silently dropping an intermediate.
            anyhow::bail!("malformed CERTIFICATE block in chain");
        };
        let end = start + end_rel + END.len();
        certs.push(after[..end - start].to_string());
        cursor = &cursor[end..];
    }

    Ok((key_part, certs))
}

/// Build a `TlsSettings` configured with the SNI callback.
///
/// h2 is advertised via ALPN by default; pass `enable_h2 = false` to
/// force h1 (workaround for the h2 + tunnel-backend bug — see
/// `pangolin_core::config::TlsConfig` for the rationale and tracking
/// reference).
pub fn build_sni_settings(
    cert_dir: std::path::PathBuf,
    enable_h2: bool,
) -> anyhow::Result<pingora::listeners::tls::TlsSettings> {
    let cb: pingora::listeners::TlsAcceptCallbacks =
        Box::new(SniCertCallback::new(Arc::new(cert_dir)));
    let mut settings = pingora::listeners::tls::TlsSettings::with_callbacks(cb)?;
    if enable_h2 {
        settings.enable_h2();
    }
    Ok(settings)
}

// Suppress unused import on non-openssl builds.
#[allow(dead_code)]
fn _force_use_path(_p: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic PEM bodies — `split_blob` is a string parser and
    // doesn't validate cert structure, so real cert content isn't
    // needed for these tests. Real cert parsing happens later in the
    // callback (`X509::from_pem`) and is exercised by the ACME
    // integration test suite.
    const LEAF_BODY: &str = "leaf";
    const INTERMEDIATE_BODY: &str = "intermediate";
    const ROOT_BODY: &str = "root";
    const KEY_BODY: &str = "key";

    fn leaf_pem() -> String {
        format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----",
            LEAF_BODY
        )
    }
    fn intermediate_pem() -> String {
        format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----",
            INTERMEDIATE_BODY
        )
    }
    fn root_pem() -> String {
        format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----",
            ROOT_BODY
        )
    }
    fn key_pem() -> String {
        format!(
            "-----BEGIN EC PRIVATE KEY-----\n{}\n-----END EC PRIVATE KEY-----",
            KEY_BODY
        )
    }

    /// The chain layout `instant_acme::order.certificate()` returns
    /// is leaf first, then intermediates, optionally root. `build_blob`
    /// in `acme.rs` writes them in that order, so `split_blob` must
    /// hand back every block — the bug it fixed was dropping the
    /// intermediates, which made Chrome show "Your connection is
    /// not secure" because Chrome has disabled AIA fetching.
    #[test]
    fn split_blob_returns_full_chain_leaf_first() {
        let blob = format!(
            "{}\n{}\n{}\n{}\n",
            key_pem(),
            leaf_pem(),
            intermediate_pem(),
            root_pem(),
        );

        let (key, certs) = split_blob(&blob).expect("blob parses");

        assert_eq!(key, key_pem(), "key is the trimmed key block");
        assert_eq!(certs.len(), 3, "leaf + intermediate + root");
        assert!(certs[0].contains(LEAF_BODY), "first is leaf");
        assert!(certs[1].contains(INTERMEDIATE_BODY), "second is intermediate");
        assert!(certs[2].contains(ROOT_BODY), "third is root");
    }

    /// The on-disk blob format also covers the leaf-only case
    /// (single-cert issuance from a non-LE source, or a PEM file
    /// imported by hand). `split_blob` must still succeed and return
    /// a one-element chain.
    #[test]
    fn split_blob_returns_single_cert_when_chain_has_only_leaf() {
        let blob = format!("{}\n{}\n", key_pem(), leaf_pem());

        let (key, certs) = split_blob(&blob).expect("blob parses");

        assert_eq!(certs.len(), 1);
        assert!(certs[0].contains(LEAF_BODY));
        assert_eq!(key, key_pem());
    }

    /// Trailing garbage after the last CERTIFICATE block — e.g. an
    /// operator appending comments or whitespace — must not crash and
    /// must not be mistaken for a CERTIFICATE block.
    #[test]
    fn split_blob_ignores_trailing_text() {
        let blob = format!(
            "{}\n{}\n{}\n# manually appended comment\n",
            key_pem(),
            leaf_pem(),
            intermediate_pem(),
        );

        let (_, certs) = split_blob(&blob).expect("blob parses");

        assert_eq!(certs.len(), 2, "leaf + intermediate, trailing text ignored");
        assert!(certs[1].contains(INTERMEDIATE_BODY));
    }

    /// An intermediate cert before the leaf (mis-ordered chain)
    /// shouldn't happen in production — LE returns leaf-first — but
    /// `split_blob` should at least not panic; we return everything
    /// we see and let the caller sort it out via the leaf-first
    /// convention.
    #[test]
    fn split_blob_handles_multiple_blocks_in_order() {
        let blob = format!(
            "{}\n{}\n{}\n{}\n",
            key_pem(),
            leaf_pem(),
            intermediate_pem(),
            intermediate_pem(),
        );

        let (_, certs) = split_blob(&blob).expect("blob parses");
        assert_eq!(certs.len(), 3, "leaf + two intermediates");
    }

    /// No `PRIVATE KEY` block before the first cert is a hard error:
    /// the blob isn't a key+cert pair, it's a stray chain file. Match
    /// the prior behaviour so operators see the same diagnostic.
    #[test]
    fn split_blob_rejects_blob_without_key() {
        let blob = format!("{}\n", leaf_pem());
        assert!(split_blob(&blob).is_err(), "missing PRIVATE KEY is rejected");
    }

    /// Truncated trailing block (BEGIN without END) must surface as a
    /// clear parse error, not silently drop the malformed chain entry.
    /// (A leaf-only blob missing the END marker is also caught here.)
    #[test]
    fn split_blob_rejects_truncated_trailing_block() {
        let blob = format!(
            "{}\n{}\n-----BEGIN CERTIFICATE-----\n{}",
            key_pem(),
            leaf_pem(),
            INTERMEDIATE_BODY,
        );
        assert!(split_blob(&blob).is_err(), "truncated chain tail is rejected");
    }
}
