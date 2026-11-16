//! QUIC transport for the tunnel (issue #153).
//!
//! QUIC is the tunnel's **primary** transport: one connection per tun,
//! one bidirectional stream per request. It removes the nested-TCP
//! penalty of the WebSocket fallback and encrypts the tunnel by
//! default — the bearer token and the operator's traffic no longer
//! cross the network in cleartext.
//!
//! ## Trust model
//!
//! The tunnel has no public hostname of its own, so the gateway
//! generates a **self-signed** certificate for its QUIC endpoint and
//! persists it (so a restart does not change it). The tun has no
//! certificate to install, so it trusts the gateway **on first use**:
//! the first connection's certificate fingerprint is pinned in a local
//! file, and every later connection must present a fingerprint from
//! that list. See [`pin`] for the store and the re-pin instructions.
//!
//! ## Authentication
//!
//! QUIC has no upgrade request to carry the tun's token in, so the tun
//! opens a **control stream** right after the handshake and writes
//! `AUTH <token> <name> <protocol-version>`. The gateway validates
//! the token against the `tun` table before it opens any request
//! stream on that connection.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use log::info;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::error::{PangolinError, Result};
use crate::tunnel::{TransportKind, TunnelStream, TunnelTransport};

use super::pin::{PinState, PinStore, PinVerdict, PinningVerifier};

/// The ALPN of the tunnel's QUIC service.
///
/// The ALPN identifies the tunnel protocol, so a tun cannot be talked
/// into a different QUIC service on the same host — and a gateway
/// that is not a tunnel gateway refuses the handshake instead of
/// misreading the bytes.
pub const TUNNEL_ALPN: &[u8] = b"pangolin-tunnel/1";

/// The name of the gateway's self-signed certificate file.
pub const QUIC_CERT_FILE: &str = "quic-cert.pem";

/// The name of the gateway's certificate key file.
pub const QUIC_KEY_FILE: &str = "quic-key.pem";

// ---------------------------------------------------------------------------
// Certificate
// ---------------------------------------------------------------------------

/// The gateway's self-signed certificate for its QUIC endpoint.
#[derive(Debug, Clone)]
pub struct QuicCert {
    /// The DER-encoded leaf certificate.
    pub cert_der: Vec<u8>,
    /// The DER-encoded PKCS#8 private key.
    pub key_der: Vec<u8>,
    /// The SHA-256 fingerprint of `cert_der`, lower-case hex.
    pub fingerprint: String,
}

impl QuicCert {
    /// Generate a fresh self-signed certificate.
    pub fn generate() -> Result<Self> {
        let generated = rcgen::generate_simple_self_signed(vec!["pangolin-tunnel".to_string()])
            .map_err(|e| PangolinError::Invalid(format!("generate the tunnel certificate: {e}")))?;
        let cert_der = generated.cert.der().to_vec();
        let key_der = generated.signing_key.serialize_der();
        let fingerprint = fingerprint(&cert_der);
        Ok(Self {
            cert_der,
            key_der,
            fingerprint,
        })
    }

    /// Load the certificate from `dir`, generating it when it is not
    /// there yet.
    ///
    /// The certificate is **persisted** so a restart does not change
    /// it: the tun pins its fingerprint, and a certificate that
    /// changed on every restart would make every tun refuse the next
    /// connection.
    pub fn load_or_generate(dir: &Path) -> Result<Self> {
        let cert_path = dir.join(QUIC_CERT_FILE);
        let key_path = dir.join(QUIC_KEY_FILE);
        if cert_path.exists() && key_path.exists() {
            let cert_pem = std::fs::read(&cert_path).map_err(|e| {
                PangolinError::Io(std::io::Error::other(format!(
                    "read {}: {e}",
                    cert_path.display()
                )))
            })?;
            let key_pem = std::fs::read(&key_path).map_err(|e| {
                PangolinError::Io(std::io::Error::other(format!(
                    "read {}: {e}",
                    key_path.display()
                )))
            })?;
            let cert_der = parse_pem_cert(&cert_pem)?;
            let key_der = parse_pem_key(&key_pem)?;
            let fingerprint = fingerprint(&cert_der);
            return Ok(Self {
                cert_der,
                key_der,
                fingerprint,
            });
        }

        let cert = Self::generate()?;
        std::fs::create_dir_all(dir).map_err(|e| {
            PangolinError::Io(std::io::Error::other(format!(
                "create {}: {e}",
                dir.display()
            )))
        })?;
        write_private(&cert_path, cert.cert_pem().as_bytes())?;
        write_private(&key_path, cert.key_pem().as_bytes())?;
        info!(
            "tunnel QUIC certificate generated at {} (fingerprint {})",
            dir.display(),
            cert.fingerprint
        );
        Ok(cert)
    }

    /// The certificate in PEM form.
    pub fn cert_pem(&self) -> String {
        pem_wrap("CERTIFICATE", &base64_encode(&self.cert_der))
    }

    /// The private key in PEM form (PKCS#8).
    pub fn key_pem(&self) -> String {
        pem_wrap("PRIVATE KEY", &base64_encode(&self.key_der))
    }
}

/// Wrap base64 `body` in a PEM block, 64 characters per line.
fn pem_wrap(label: &str, body: &str) -> String {
    let mut out = format!("-----BEGIN {label}-----\n");
    for chunk in body.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

/// Write a file that only the owner can read.
///
/// The certificate is public, but the key next to it is not — and the
/// operator may run the gateway as a different user than the one
/// that reads the files.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes).map_err(|e| {
        PangolinError::Io(std::io::Error::other(format!(
            "write {}: {e}",
            path.display()
        )))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// The SHA-256 fingerprint of a DER certificate, lower-case hex.
///
/// This is the value both sides log and the value the tun pins. It
/// identifies the certificate, not the key: re-issuing a certificate
/// with the same key produces a different fingerprint, which is what
/// we want — the operator has to re-pin deliberately.
pub fn fingerprint(cert_der: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(cert_der);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Parse the first PEM block of `label` out of a PEM file.
fn parse_pem_block(pem: &[u8], label: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    let text = String::from_utf8_lossy(pem);
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let start = text.find(&begin)? + begin.len();
    let stop = text[start..].find(&end)? + start;
    let body: String = text[start..stop]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    base64::engine::general_purpose::STANDARD.decode(body).ok()
}

fn parse_pem_cert(pem: &[u8]) -> Result<Vec<u8>> {
    parse_pem_block(pem, "CERTIFICATE").ok_or_else(|| {
        PangolinError::Invalid("the tunnel certificate file has no CERTIFICATE block".into())
    })
}

fn parse_pem_key(pem: &[u8]) -> Result<Vec<u8>> {
    parse_pem_block(pem, "PRIVATE KEY").ok_or_else(|| {
        PangolinError::Invalid("the tunnel key file has no PRIVATE KEY block".into())
    })
}

// ---------------------------------------------------------------------------
// The bidirectional stream
// ---------------------------------------------------------------------------

/// A QUIC bidirectional stream presented as a byte stream.
///
/// `quinn` splits a bidirectional stream into a `SendStream` and a
/// `RecvStream`; the tunnel needs one object that is both readable
/// and writable.
pub struct QuicBiStream {
    send: quinn::SendStream,
    recv: quinn::RecvStream,
}

impl QuicBiStream {
    /// Combine a `SendStream` and a `RecvStream` into one byte
    /// stream.
    pub fn new(send: quinn::SendStream, recv: quinn::RecvStream) -> Self {
        Self { send, recv }
    }
}

impl std::fmt::Debug for QuicBiStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuicBiStream").finish()
    }
}

impl AsyncRead for QuicBiStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // Call the trait method explicitly: quinn's inherent
        // `poll_read` returns its own error type, not `std::io::Error`.
        AsyncRead::poll_read(Pin::new(&mut self.recv), cx, buf)
    }
}

impl AsyncWrite for QuicBiStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        AsyncWrite::poll_write(Pin::new(&mut self.send), cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        AsyncWrite::poll_flush(Pin::new(&mut self.send), cx)
    }

    /// Half-close the send side. The peer sees the end of our bytes
    /// and may keep sending — which is what the relay's half-close
    /// handling needs.
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        AsyncWrite::poll_shutdown(Pin::new(&mut self.send), cx)
    }
}

// ---------------------------------------------------------------------------
// The client (the tun)
// ---------------------------------------------------------------------------

/// The tun's QUIC transport.
#[derive(Debug, Clone)]
pub struct QuicClientTransport {
    /// The endpoint is kept alive for the lifetime of the transport:
    /// dropping it tears the connection down.
    _endpoint: quinn::Endpoint,
    connection: quinn::Connection,
}

impl QuicClientTransport {
    /// Connect to a gateway's QUIC endpoint and check its certificate
    /// against the pin store.
    ///
    /// On the very first connection the pin store is empty: the
    /// presented fingerprint is **pinned** and the pin is persisted.
    /// Afterwards only a fingerprint from the list is accepted; a
    /// mismatch is refused with a message naming both fingerprints
    /// and explaining how to re-pin.
    pub async fn connect(
        addr: SocketAddr,
        server_name: &str,
        pins: &PinStore,
    ) -> Result<(Self, bool)> {
        let state = Arc::new(PinState::default());
        let verifier = PinningVerifier::new(pins.pins(), state.clone(), pins.path().to_path_buf());
        // The certificate is self-signed by design and the tunnel has
        // no public hostname, so the verifier replaces rustls' chain
        // and hostname checks with the fingerprint pin.
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| PangolinError::Invalid(format!("tls protocol versions: {e}")))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();
        tls.alpn_protocols = vec![TUNNEL_ALPN.to_vec()];
        let quic_config = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
            .map_err(|e| PangolinError::Invalid(format!("quic client config: {e}")))?;
        let mut config = quinn::ClientConfig::new(Arc::new(quic_config));
        // The tunnel session is long-lived: the idle timeout is
        // paired with a keep-alive so a quiet connection stays open.
        config.transport_config(Arc::new(transport_config()));

        let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse().unwrap())
            .map_err(|e| PangolinError::Io(std::io::Error::other(format!("quic endpoint: {e}"))))?;
        endpoint.set_default_client_config(config);

        let connecting = endpoint
            .connect(addr, server_name)
            .map_err(|e| PangolinError::Io(std::io::Error::other(format!("quic connect: {e}"))))?;
        let connection = connecting.await.map_err(|e| {
            PangolinError::Io(std::io::Error::other(format!("quic handshake: {e}")))
        })?;

        // The handshake is over. Now decide whether we trust the
        // certificate that was presented.
        let presented = state.presented();
        let newly_pinned = match pins.verdict(presented.as_deref()) {
            PinVerdict::Accepted => false,
            PinVerdict::Pin => {
                let Some(fingerprint) = presented.as_deref() else {
                    return Err(PangolinError::Invalid(
                        "the gateway presented no certificate, so there is nothing to pin".into(),
                    ));
                };
                pins.add(fingerprint)?;
                info!("tunnel: pinned the gateway's QUIC certificate (fingerprint {fingerprint})");
                true
            }
            PinVerdict::Refused(msg) => {
                connection.close(0u32.into(), msg.as_bytes());
                return Err(PangolinError::Invalid(msg));
            }
        };

        Ok((
            Self {
                _endpoint: endpoint,
                connection,
            },
            newly_pinned,
        ))
    }

    /// The underlying QUIC connection.
    pub fn connection(&self) -> &quinn::Connection {
        &self.connection
    }
}

#[async_trait::async_trait]
impl TunnelTransport for QuicClientTransport {
    async fn open_stream(&self) -> Result<TunnelStream> {
        let (send, recv) = self.connection.open_bi().await.map_err(|e| {
            PangolinError::Io(std::io::Error::other(format!("quic open stream: {e}")))
        })?;
        Ok(Box::new(QuicBiStream { send, recv }))
    }

    async fn accept_stream(&self) -> Option<Result<TunnelStream>> {
        // quinn's `accept_bi` yields a `Result`, not an `Option`: a
        // closed connection is an error, not an end-of-stream. The
        // caller treats an error as the end of the session.
        match self.connection.accept_bi().await {
            Ok((send, recv)) => Some(Ok(Box::new(QuicBiStream { send, recv }))),
            Err(e) => Some(Err(PangolinError::Io(std::io::Error::other(format!(
                "quic accept stream: {e}"
            ))))),
        }
    }

    fn kind(&self) -> TransportKind {
        TransportKind::Quic
    }
}

// ---------------------------------------------------------------------------
// The server (the gateway)
// ---------------------------------------------------------------------------

/// The gateway's QUIC transport for one tun.
#[derive(Debug, Clone)]
pub struct QuicServerTransport {
    connection: quinn::Connection,
}

impl QuicServerTransport {
    /// Wrap an accepted QUIC connection.
    pub fn new(connection: quinn::Connection) -> Self {
        Self { connection }
    }

    /// The underlying QUIC connection.
    pub fn connection(&self) -> &quinn::Connection {
        &self.connection
    }
}

#[async_trait::async_trait]
impl TunnelTransport for QuicServerTransport {
    async fn open_stream(&self) -> Result<TunnelStream> {
        let (send, recv) = self.connection.open_bi().await.map_err(|e| {
            PangolinError::Io(std::io::Error::other(format!("quic open stream: {e}")))
        })?;
        Ok(Box::new(QuicBiStream { send, recv }))
    }

    async fn accept_stream(&self) -> Option<Result<TunnelStream>> {
        // quinn's `accept_bi` yields a `Result`, not an `Option`: a
        // closed connection is an error, not an end-of-stream. The
        // caller treats an error as the end of the session.
        match self.connection.accept_bi().await {
            Ok((send, recv)) => Some(Ok(Box::new(QuicBiStream { send, recv }))),
            Err(e) => Some(Err(PangolinError::Io(std::io::Error::other(format!(
                "quic accept stream: {e}"
            ))))),
        }
    }

    fn kind(&self) -> TransportKind {
        TransportKind::Quic
    }
}

// ---------------------------------------------------------------------------
// The gateway's endpoint
// ---------------------------------------------------------------------------

/// Bind the gateway's QUIC endpoint on `addr`.
///
/// The endpoint speaks the tunnel's ALPN only, so it cannot be
/// confused with another QUIC service on the same port.
pub fn bind_endpoint(addr: SocketAddr, cert: &QuicCert) -> Result<quinn::Endpoint> {
    let cert_der = rustls::pki_types::CertificateDer::from(cert.cert_der.clone());
    let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(
        rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_der.clone()),
    );
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut server_config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| PangolinError::Invalid(format!("tls protocol versions: {e}")))?
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .map_err(|e| {
            PangolinError::Invalid(format!("the tunnel certificate is not usable: {e}"))
        })?;
    server_config.alpn_protocols = vec![TUNNEL_ALPN.to_vec()];
    let quic_config = quinn::crypto::rustls::QuicServerConfig::try_from(server_config)
        .map_err(|e| PangolinError::Invalid(format!("quic server config: {e}")))?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(quic_config));
    // The tunnel session is long-lived: the idle timeout is paired
    // with a keep-alive so a quiet connection stays open.
    config.transport_config(Arc::new(transport_config()));
    quinn::Endpoint::server(config, addr).map_err(|e| {
        PangolinError::Io(std::io::Error::other(format!(
            "bind the quic endpoint: {e}"
        )))
    })
}

/// Where the gateway keeps the tunnel's certificate.
pub fn certificate_dir(cert_dir: &Path) -> PathBuf {
    cert_dir.join("tunnel")
}

/// The QUIC transport settings for a tunnel session.
///
/// A tunnel session is long-lived — a WebSocket connection can sit
/// idle for minutes between frames. QUIC's default idle timeout
/// would close such a session, so the timeout is paired with a
/// keep-alive interval that keeps the connection warm: the same
/// shape as the yamux session's keepalive on the WebSocket
/// transport.
///
/// The idle timeout is still what notices a peer that disappeared:
/// the keep-alive PINGs go unanswered and the connection times out.
pub fn transport_config() -> quinn::TransportConfig {
    let mut transport = quinn::TransportConfig::default();
    transport.keep_alive_interval(Some(std::time::Duration::from_secs(10)));
    transport.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(std::time::Duration::from_secs(30))
            .unwrap_or(quinn::IdleTimeout::try_from(std::time::Duration::from_secs(10)).unwrap()),
    ));
    transport
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A generated certificate is usable and has a stable fingerprint.
    #[test]
    fn generated_certificate_has_a_fingerprint() {
        let cert = QuicCert::generate().unwrap();
        assert_eq!(cert.fingerprint.len(), 64, "sha-256 hex is 64 chars");
        assert!(cert.fingerprint.chars().all(|c| c.is_ascii_hexdigit()));
        // The fingerprint is the SHA-256 of the DER bytes.
        assert_eq!(fingerprint(&cert.cert_der), cert.fingerprint);
    }

    /// Two generated certificates differ — otherwise the pin would
    /// not identify anything.
    #[test]
    fn generated_certificates_differ() {
        let a = QuicCert::generate().unwrap();
        let b = QuicCert::generate().unwrap();
        assert_ne!(a.fingerprint, b.fingerprint);
    }

    /// The certificate survives a round-trip through the PEM files it
    /// is persisted in — a restart must not change the fingerprint.
    #[test]
    fn certificate_pem_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let first = QuicCert::load_or_generate(dir.path()).unwrap();
        let second = QuicCert::load_or_generate(dir.path()).unwrap();
        assert_eq!(first.fingerprint, second.fingerprint);
        assert_eq!(first.cert_der, second.cert_der);
        assert_eq!(first.key_der, second.key_der);
    }

    /// A deleted certificate is regenerated — but then the fingerprint
    /// changes, which is exactly why the tun's pin has to be updated.
    #[test]
    fn a_deleted_certificate_is_regenerated() {
        let dir = tempfile::tempdir().unwrap();
        let first = QuicCert::load_or_generate(dir.path()).unwrap();
        std::fs::remove_file(dir.path().join(QUIC_CERT_FILE)).unwrap();
        std::fs::remove_file(dir.path().join(QUIC_KEY_FILE)).unwrap();
        let second = QuicCert::load_or_generate(dir.path()).unwrap();
        assert_ne!(first.fingerprint, second.fingerprint);
    }
}
