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
//!
//! ## Per-SNI dynamic ALPN
//!
//! [`build_sni_settings`] installs an ALPN selection callback that
//! decides HTTP/2 vs HTTP/1.1 per connection based on the SNI:
//!
//! - If the SNI resolves to a site with a `tun_name:` backend prefix
//!   (i.e. the request will be proxied through a yamux tunnel
//!   session), the listener offers **HTTP/1.1 only** — this avoids
//!   the `tokio-yamux 0.3.18` stream-state race that breaks the
//!   h2 + tunnel path (issue #66 / commit `0c35ede`).
//! - Otherwise the listener follows `config.tls.enable_h2`.
//!
//! The decision is per-connection, so the operator-facing `tls.enable_h2`
//! flag still controls the non-tunnel default; the runtime override
//! for tunnel sites is transparent to clients — no 4xx, no 421, no
//! manual retry, just a fallback to h1 inside the handshake.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use openssl::pkey::PKey;
use openssl::ssl::AlpnError;
use openssl::x509::X509;
use pangolin_core::App;
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

/// Build a `TlsSettings` configured with the SNI cert callback AND a
/// per-SNI dynamic ALPN selection callback.
///
/// The ALPN callback is installed via `SslAcceptorBuilder::set_alpn_select_callback`
/// (reachable through `TlsSettings`'s `Deref<Target = SslAcceptorBuilder>`).
/// It is called by openssl/boringssl during the TLS handshake, in a sync
/// C context, on a pingora tokio worker thread. The callback:
///
/// 1. Reads the SNI from the in-progress `SslRef`.
/// 2. Looks up the SNI in `app.tunnel_domains` (sync `parking_lot` mirror
///    of the in-memory `Indexes`; the mirror is rebuilt atomically on
///    every admin-triggered `App::reload_indexes`).
/// 3. Picks the target protocol: `http/1.1` for tunnel sites, `h2` for
///    non-tunnel sites if `config.tls.enable_h2`, otherwise `http/1.1`.
/// 4. Walks the client-offered ALPN list and returns the first match;
///    falls back to the other protocol if the target is not offered.
///
/// We do NOT call `TlsSettings::enable_h2()` here — that would install
/// a static h2-preferring ALPN callback that ignores SNI. Dynamic
/// per-SNI selection is the whole point of this function.
pub fn build_sni_settings(
    cert_dir: std::path::PathBuf,
    app: Arc<App>,
) -> anyhow::Result<pingora::listeners::tls::TlsSettings> {
    let cb: pingora::listeners::TlsAcceptCallbacks =
        Box::new(SniCertCallback::new(Arc::new(cert_dir)));
    let mut settings = pingora::listeners::tls::TlsSettings::with_callbacks(cb)?;

    // Install per-SNI dynamic ALPN selection.
    install_dynamic_alpn(&mut settings, app);

    Ok(settings)
}

/// Install the per-SNI ALPN selection callback. See module-level docs
/// and [`build_sni_settings`].
fn install_dynamic_alpn(settings: &mut pingora::listeners::tls::TlsSettings, app: Arc<App>) {
    // Capture the config snapshot once. `app.config` is set at startup
    // and never mutated, so this is safe; if it ever changes, the listener
    // would need to be rebuilt anyway (cert dir, SNI map, etc.).
    let global_enable_h2 = app.config.tls.enable_h2;

    settings.set_alpn_select_callback(move |ssl_ref, alpn_in| {
        // 1. Pull SNI. If the client didn't send SNI, we have no way to
        //    route; return NOACK so openssl closes the handshake. The cert
        //    callback will also fail because it can't find a cert blob.
        let sni = match ssl_ref.servername(NameType::HOST_NAME) {
            Some(s) => s.to_lowercase(),
            None => return Err(AlpnError::NOACK),
        };

        // 2. Sync lookup: is this SNI a tunnel site?
        let is_tunnel = {
            let guard = app.tunnel_domains.read();
            pangolin_core::index::host_matches_set(&guard, &sni)
        };

        // 3. Pick the target protocol.
        let target: &[u8] = if is_tunnel {
            b"http/1.1"
        } else if global_enable_h2 {
            b"h2"
        } else {
            b"http/1.1"
        };

        // 4. Walk the client-offered ALPN list, prefer the target; if the
        //    target is not offered, fall back to the other protocol so
        //    h1-only clients (curl --http1.1) still work.
        if let Some(p) = pick_protocol(alpn_in, target) {
            return Ok(p);
        }
        let fallback: &[u8] = if target == b"h2" { b"http/1.1" } else { b"h2" };
        if let Some(p) = pick_protocol(alpn_in, fallback) {
            return Ok(p);
        }

        // Client offered neither h2 nor http/1.1 — return NOACK so openssl
        // sends a fatal alert. This matches `prefer_h2` / `h1_only` in
        // vendored pingora.
        Err(AlpnError::NOACK)
    });
}

/// Walk an ALPN wire-format list (`len || proto || len || proto …`)
/// and return the first protocol matching `target`. The returned slice
/// has the same lifetime as `alpn_in`, so it satisfies the
/// `set_alpn_select_callback` lifetime contract.
fn pick_protocol<'a>(alpn_in: &'a [u8], target: &[u8]) -> Option<&'a [u8]> {
    let mut bytes = alpn_in;
    while !bytes.is_empty() {
        let len = bytes[0] as usize;
        // Defensive: malformed input. The ALPN spec requires
        // `0 < len <= 255` and that the rest of the buffer can hold
        // the protocol bytes. Bail out on any violation.
        if len == 0 || 1 + len > bytes.len() {
            return None;
        }
        let proto = &bytes[1..1 + len];
        if proto == target {
            return Some(proto);
        }
        bytes = &bytes[1 + len..];
    }
    None
}

// Suppress unused import on non-openssl builds.
#[allow(dead_code)]
fn _force_use_path(_p: &Path) {}

#[cfg(test)]
mod tests {
    use super::pick_protocol;

    /// Build a wire-format ALPN list from a slice of protocol names.
    fn alpn(items: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for p in items {
            assert!(
                !p.is_empty() && p.len() <= 255,
                "alpn proto len out of range"
            );
            out.push(p.len() as u8);
            out.extend_from_slice(p);
        }
        out
    }

    #[test]
    fn pick_protocol_first_match() {
        let wire = alpn(&[b"h2", b"http/1.1"]);
        assert_eq!(pick_protocol(&wire, b"h2"), Some(&b"h2"[..]));
        assert_eq!(pick_protocol(&wire, b"http/1.1"), Some(&b"http/1.1"[..]));
    }

    #[test]
    fn pick_protocol_reverse_order() {
        // Client prefers h1 first; the callback should still return h2
        // if h2 is the target.
        let wire = alpn(&[b"http/1.1", b"h2"]);
        assert_eq!(pick_protocol(&wire, b"h2"), Some(&b"h2"[..]));
    }

    #[test]
    fn pick_protocol_miss_returns_none() {
        let wire = alpn(&[b"h2", b"http/1.1"]);
        assert_eq!(pick_protocol(&wire, b"spdy/3"), None);
    }

    #[test]
    fn pick_protocol_empty_input() {
        assert_eq!(pick_protocol(&[], b"h2"), None);
    }

    #[test]
    fn pick_protocol_ignores_malformed_entries() {
        // len=0 is malformed; the walker should bail out and return None
        // rather than panic or walk past the end of the buffer.
        let wire = vec![0u8];
        assert_eq!(pick_protocol(&wire, b"h2"), None);

        // len=5 but only 1 byte left is also malformed.
        let wire = vec![5u8, b'h'];
        assert_eq!(pick_protocol(&wire, b"h2"), None);
    }

    #[test]
    fn pick_protocol_h2_05d8_aka_h2c() {
        // h2c (HTTP/2 over cleartext) is sometimes offered; we treat it
        // as a non-match (we only do TLS h2). The fallback path in
        // `install_dynamic_alpn` then falls back to http/1.1.
        let wire = alpn(&[b"h2c", b"http/1.1"]);
        assert_eq!(pick_protocol(&wire, b"h2"), None);
        assert_eq!(pick_protocol(&wire, b"http/1.1"), Some(&b"http/1.1"[..]));
    }
}
