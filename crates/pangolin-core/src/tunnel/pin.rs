//! The tun's trust-on-first-use pin store (issue #153).
//!
//! The gateway's QUIC endpoint uses a self-signed certificate it
//! generates itself, and the operator has no certificate to install on
//! the tun. The tun therefore trusts the gateway **on first use**: the
//! first successful connection's fingerprint is pinned in a local file
//! and every later connection must present a fingerprint from that
//! list.
//!
//! The file holds a **list**, not a single value, so a deployment with
//! several gateway nodes — each with its own generated certificate —
//! works without the operator copying certificates between nodes.
//!
//! ## The weak spot
//!
//! The first contact is the weak point of any trust-on-first-use
//! scheme: a middleman present at that moment would be pinned instead
//! of the gateway. Both sides therefore log the fingerprint when the
//! pin is created, so the operator can compare the tun's log with the
//! gateway's log afterwards.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use log::info;
use parking_lot::Mutex;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as RustlsError, SignatureScheme};

use crate::error::{PangolinError, Result};

use super::quic::fingerprint;

/// What the pin store says about a presented certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinVerdict {
    /// The fingerprint is in the list — the certificate is trusted.
    Accepted,
    /// The list is empty — this is the first contact. The caller
    /// pins the fingerprint and persists it.
    Pin,
    /// The fingerprint is not in the list. The message names the
    /// pinned fingerprints, the presented one, and how to re-pin.
    Refused(String),
}

/// The tun's list of trusted gateway certificate fingerprints.
///
/// The list lives in a plain text file, one fingerprint per line, so
/// an operator can inspect it and edit it by hand to re-pin.
#[derive(Debug, Clone)]
pub struct PinStore {
    path: PathBuf,
    pins: Arc<Mutex<Vec<String>>>,
}

impl PinStore {
    /// Open (or create) the pin store at `path`.
    ///
    /// A missing file is an **empty** list, which is the trust-on-first-use
    /// starting point — not an error.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let pins = match std::fs::read_to_string(&path) {
            Ok(text) => text
                .lines()
                .map(|l| l.trim().to_ascii_lowercase())
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                return Err(PangolinError::Io(std::io::Error::new(
                    e.kind(),
                    format!("read the pin file {}: {e}", path.display()),
                )));
            }
        };
        Ok(Self {
            path,
            pins: Arc::new(Mutex::new(pins)),
        })
    }

    /// The path of the pin file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The pinned fingerprints.
    pub fn pins(&self) -> Vec<String> {
        self.pins.lock().clone()
    }

    /// Decide what to do with a presented certificate fingerprint.
    pub fn verdict(&self, presented: Option<&str>) -> PinVerdict {
        let Some(presented) = presented else {
            return PinVerdict::Refused(
                "the gateway presented no certificate, so there is nothing to pin".to_string(),
            );
        };
        let pins = self.pins.lock();
        if pins.is_empty() {
            return PinVerdict::Pin;
        }
        if pins.iter().any(|p| p == presented) {
            return PinVerdict::Accepted;
        }
        PinVerdict::Refused(refusal_message(&pins, presented, &self.path))
    }

    /// Add a fingerprint to the list and persist it.
    pub fn add(&self, fingerprint: &str) -> Result<()> {
        let fingerprint = fingerprint.trim().to_ascii_lowercase();
        if fingerprint.is_empty() {
            return Err(PangolinError::Invalid(
                "refusing to pin an empty fingerprint".into(),
            ));
        }
        {
            let mut pins = self.pins.lock();
            if pins.contains(&fingerprint) {
                return Ok(());
            }
            pins.push(fingerprint.clone());
        }
        self.persist()
    }

    /// Write the list back to the pin file.
    fn persist(&self) -> Result<()> {
        let pins = self.pins.lock();
        let mut body = String::from(
            "# Pangolin tunnel — trusted gateway certificate fingerprints.\n\
             # One SHA-256 fingerprint (lower-case hex) per line.\n\
             # Delete a line to stop trusting that gateway; add a line to trust another one.\n",
        );
        for pin in pins.iter() {
            body.push_str(pin);
            body.push('\n');
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                PangolinError::Io(std::io::Error::new(
                    e.kind(),
                    format!("create {}: {e}", parent.display()),
                ))
            })?;
        }
        std::fs::write(&self.path, body).map_err(|e| {
            PangolinError::Io(std::io::Error::new(
                e.kind(),
                format!("write the pin file {}: {e}", self.path.display()),
            ))
        })
    }
}

/// The message the tun shows when a certificate is not pinned.
///
/// It names the pinned fingerprints **and** the presented one, and it
/// says how to re-pin — the operator has to be able to fix this
/// without reading the source.
fn refusal_message(pins: &[String], presented: &str, path: &Path) -> String {
    format!(
        "the gateway's QUIC certificate is not trusted: it presented the fingerprint {presented}, \
         but this tun only trusts {}. To trust the new certificate, remove the stale \
         fingerprint(s) from {} and reconnect — the tun then pins the new one.",
        if pins.len() == 1 {
            format!("the fingerprint {}", pins[0])
        } else {
            format!("the fingerprints {}", pins.join(", "))
        },
        path.display()
    )
}

/// The state the pinning verifier shares with the caller: the
/// fingerprint of the certificate the peer presented.
#[derive(Debug, Default)]
pub struct PinState {
    presented: Mutex<Option<String>>,
}

impl PinState {
    /// The fingerprint the peer presented, if any.
    pub fn presented(&self) -> Option<String> {
        self.presented.lock().clone()
    }
}

/// A rustls verifier that accepts the gateway's self-signed
/// certificate **only** when its fingerprint is pinned.
///
/// The chain and the hostname are not checked: the certificate is
/// self-signed by design and the tunnel has no public hostname. What
/// *is* checked is the fingerprint — that is the whole trust model.
#[derive(Debug)]
pub struct PinningVerifier {
    /// Shared with the caller so it can read the presented fingerprint
    /// once the handshake is over.
    pub state: Arc<PinState>,
    /// The pinned fingerprints. Empty means "first contact" — the
    /// certificate is accepted and the caller pins it.
    pins: Arc<Mutex<Vec<String>>>,
    /// Where the pin file lives — the refusal message tells the
    /// operator which file to edit.
    pin_path: PathBuf,
}

impl PinningVerifier {
    /// Build a verifier for a set of pinned fingerprints.
    pub fn new(pins: Vec<String>, state: Arc<PinState>, pin_path: PathBuf) -> Self {
        Self {
            state,
            pins: Arc::new(Mutex::new(pins)),
            pin_path,
        }
    }
}

impl ServerCertVerifier for PinningVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, RustlsError> {
        let presented = fingerprint(end_entity.as_ref());
        *self.state.presented.lock() = Some(presented.clone());
        let pins = self.pins.lock();
        if pins.is_empty() {
            // First contact: the caller pins this fingerprint once the
            // handshake succeeded.
            return Ok(ServerCertVerified::assertion());
        }
        if pins.contains(&presented) {
            return Ok(ServerCertVerified::assertion());
        }
        Err(RustlsError::General(refusal_message(
            &pins,
            &presented,
            &self.pin_path,
        )))
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, RustlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, RustlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Log the fingerprint of a newly created pin.
///
/// Both sides log it: the tun when it pins, the gateway when it
/// generates its certificate. An operator compares the two logs to
/// confirm that the first contact was not intercepted.
pub fn log_pin_created(fingerprint: &str, path: &Path) {
    info!(
        "tunnel: pinned the gateway's QUIC certificate {fingerprint} in {}",
        path.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(pins: &[&str]) -> (PinStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tun.yml.pins");
        if !pins.is_empty() {
            std::fs::write(&path, pins.join("\n")).unwrap();
        }
        (PinStore::open(&path).unwrap(), dir)
    }

    /// The first contact pins: an empty list asks the caller to pin.
    #[test]
    fn an_empty_list_asks_for_a_pin() {
        let (store, _dir) = store_with(&[]);
        assert_eq!(store.verdict(Some("aa")), PinVerdict::Pin);
    }

    /// A pinned fingerprint is accepted.
    #[test]
    fn a_pinned_fingerprint_is_accepted() {
        let (store, _dir) = store_with(&["aa"]);
        assert_eq!(store.verdict(Some("aa")), PinVerdict::Accepted);
    }

    /// A different fingerprint is refused, and the refusal names both
    /// the pinned and the presented fingerprint and says how to re-pin.
    #[test]
    fn a_different_fingerprint_is_refused_with_both_fingerprints() {
        let (store, _dir) = store_with(&["aa"]);
        let PinVerdict::Refused(msg) = store.verdict(Some("bb")) else {
            panic!("expected a refusal");
        };
        assert!(
            msg.contains("aa"),
            "the pinned fingerprint must be named: {msg}"
        );
        assert!(
            msg.contains("bb"),
            "the presented fingerprint must be named: {msg}"
        );
        assert!(
            msg.contains("pin"),
            "the message must say how to re-pin: {msg}"
        );
    }

    /// The list holds several fingerprints, so a multi-node deployment
    /// works without copying certificates between nodes.
    #[test]
    fn the_list_holds_several_fingerprints() {
        let (store, _dir) = store_with(&["aa", "bb"]);
        assert_eq!(store.verdict(Some("aa")), PinVerdict::Accepted);
        assert_eq!(store.verdict(Some("bb")), PinVerdict::Accepted);
        assert!(matches!(store.verdict(Some("cc")), PinVerdict::Refused(_)));
    }

    /// A pin survives a restart: the file is the source of truth.
    #[test]
    fn a_pin_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tun.yml.pins");
        let store = PinStore::open(&path).unwrap();
        store.add("aa").unwrap();
        let reopened = PinStore::open(&path).unwrap();
        assert_eq!(reopened.pins(), vec!["aa".to_string()]);
        assert_eq!(reopened.verdict(Some("aa")), PinVerdict::Accepted);
    }

    /// Comments and blank lines in the pin file are ignored, so an
    /// operator can annotate it.
    #[test]
    fn the_pin_file_tolerates_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tun.yml.pins");
        std::fs::write(&path, "# a comment\n\naa\n  bb  \n").unwrap();
        let store = PinStore::open(&path).unwrap();
        assert_eq!(store.pins(), vec!["aa".to_string(), "bb".to_string()]);
    }

    /// The same fingerprint is not added twice.
    #[test]
    fn adding_the_same_fingerprint_twice_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tun.yml.pins");
        let store = PinStore::open(&path).unwrap();
        store.add("aa").unwrap();
        store.add("AA").unwrap();
        assert_eq!(store.pins(), vec!["aa".to_string()]);
    }
}
