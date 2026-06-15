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
        let (key_pem, cert_pem) = match split_blob(&blob) {
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
        let cert = match X509::from_pem(cert_pem.as_bytes()) {
            Ok(c) => c,
            Err(e) => {
                log::error!("TLS: certificate parse failed: {}", e);
                return;
            }
        };

        // Hand the cert + key to the in-progress handshake. Any error
        // here aborts the handshake, which is what we want.
        if let Err(e) = pingora_openssl::ext::ssl_use_certificate(ssl, &cert) {
            log::error!("TLS: ssl_use_certificate failed: {}", e);
            return;
        }
        if let Err(e) = pingora_openssl::ext::ssl_use_private_key(ssl, &pkey) {
            log::error!("TLS: ssl_use_private_key failed: {}", e);
            return;
        }
        log::debug!("TLS: cert installed for SNI '{}'", sni);
    }
}

/// Split a combined key+cert PEM blob (autocert DirCache native format:
/// key block first, then one or more certificate blocks) into the
/// key PEM and the leaf cert PEM.
fn split_blob(blob: &str) -> anyhow::Result<(String, String)> {
    // Locate the first CERTIFICATE block — that's the leaf.
    let cert_start = blob
        .find("-----BEGIN CERTIFICATE-----")
        .ok_or_else(|| anyhow::anyhow!("no CERTIFICATE block in blob"))?;
    let cert_end_rel = blob[cert_start..]
        .find("-----END CERTIFICATE-----")
        .ok_or_else(|| anyhow::anyhow!("malformed CERTIFICATE block"))?;
    let cert_end = cert_start + cert_end_rel + "-----END CERTIFICATE-----".len();

    // The key is everything before the first CERTIFICATE block. Strip
    // trailing whitespace.
    let key_part = blob[..cert_start].trim_end().to_string();
    let cert_part = blob[cert_start..cert_end].to_string();

    if !key_part.contains("PRIVATE KEY") {
        anyhow::bail!("no PRIVATE KEY block before the certificate");
    }
    Ok((key_part, cert_part))
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
