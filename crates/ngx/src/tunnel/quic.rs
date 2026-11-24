//! The gateway's QUIC tunnel endpoint (issue #153).
//!
//! QUIC is the tunnel's **primary** transport. The listener binds the
//! same port number as `tunnel.addr` — but over UDP, which is a
//! separate port space, so it needs no new configuration key.
//!
//! A failure to bind the UDP port is **not** fatal: the gateway logs a
//! warning and keeps serving the WebSocket/TCP fallback. Refusing to
//! start would turn a degraded mode into an outage.
//!
//! ## The handshake
//!
//! QUIC has no upgrade request to carry the tun's token in, so the
//! tun opens a **control stream** right after the handshake and writes
//!
//! ```text
//! tun → gateway:  AUTH <token> <name> <protocol-version>\r\n
//! gateway → tun:  OK <protocol-version>\r\n      (or)   ERR <reason>\r\n
//! ```
//!
//! The gateway validates the token against the `tun` table **before**
//! it opens any request stream on that connection. An invalid token
//! closes the connection and is logged the same way an invalid
//! WebSocket token is.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use log::{info, warn};
use tokio::io::AsyncWriteExt;

use pangolin_tun::{
    QuicBiStream, QuicCert, QuicServerTransport, TUNNEL_PROTOCOL_VERSION, TunnelTransport,
    bind_endpoint_with_socket, decode_auth, encode_auth_reply, protocol_version_mismatch,
    read_crlf_line,
};

use crate::App;
use crate::runtime::{Service, ServiceContext};
use crate::tunnel::validate_token;

/// How long the gateway waits for the tun's `AUTH` control line.
///
/// A connection that never authenticates must not hold a task (and a
/// QUIC connection) forever: the transport's keep-alive would keep it
/// open indefinitely.
const AUTH_TIMEOUT_SECS: u64 = 10;

/// Bind the QUIC endpoint on a socket that asks for `SO_REUSEPORT`.
///
/// The endpoint is built on a socket this crate binds itself
/// (`pangolin_tun::bind_endpoint_with_socket`) because the option has
/// to be set **before** the bind — quinn's own `bind_endpoint` binds
/// the socket itself and cannot be told to ask for it.
///
/// The QUIC listener shares its port number with the tunnel's TCP
/// listener, and it is the transport the gateway prefers: a QUIC
/// endpoint that cannot bind leaves the gateway on the WebSocket/TCP
/// fallback, and that downgrade is persistent (the tun keeps using
/// the transport it negotiated). So the QUIC listener has to be part
/// of the handover group like every other listener (issue #233).
fn bind_quic_endpoint(addr: SocketAddr, cert: &QuicCert) -> anyhow::Result<quinn::Endpoint> {
    let socket = crate::reuseport::bind_udp(addr)
        .map_err(|e| anyhow::anyhow!("bind the quic socket on udp {addr}: {e}"))?;
    // The endpoint is told the socket's **own** address: the crate
    // cross-checks it against the socket and refuses a mismatch. The
    // socket's address is the only correct answer — it also covers
    // the "port 0" case, where the kernel picked the port.
    let local = socket
        .local_addr()
        .map_err(|e| anyhow::anyhow!("the quic socket has no local address: {e}"))?;
    bind_endpoint_with_socket(local, socket, cert)
        .map_err(|e| anyhow::anyhow!("bind the quic endpoint on udp {local}: {e}"))
}

/// The gateway's QUIC tunnel service.
pub struct QuicService {
    /// The UDP address to bind — the same port number as
    /// `tunnel.addr`.
    addr: SocketAddr,
    /// Where the generated certificate lives.
    cert_dir: PathBuf,
}

impl QuicService {
    /// Build the service.
    pub fn new(addr: SocketAddr, cert_dir: PathBuf) -> Self {
        Self { addr, cert_dir }
    }
}

#[async_trait::async_trait]
impl Service for QuicService {
    fn name(&self) -> &'static str {
        "tunnel-quic"
    }

    async fn run(&self, ctx: ServiceContext) -> anyhow::Result<()> {
        let cert = match QuicCert::load_or_generate(&self.cert_dir) {
            Ok(cert) => cert,
            Err(e) => {
                // A degraded mode, not an outage: the WebSocket/TCP
                // fallback still works.
                warn!(
                    "tunnel: the QUIC certificate could not be prepared ({}); \
                     serving the WebSocket/TCP tunnel only",
                    e
                );
                return Ok(());
            }
        };
        // The operator compares this fingerprint with the tun's log to
        // confirm that the first contact was not intercepted.
        info!(
            "tunnel: QUIC certificate fingerprint {} ({})",
            cert.fingerprint,
            self.cert_dir.display()
        );

        let endpoint = match bind_quic_endpoint(self.addr, &cert) {
            Ok(endpoint) => endpoint,
            Err(e) => {
                warn!(
                    "tunnel: could not bind the QUIC listener on udp {} ({}); \
                     serving the WebSocket/TCP tunnel only",
                    self.addr, e
                );
                return Ok(());
            }
        };
        info!("tunnel: QUIC listener on udp {}", self.addr);

        loop {
            tokio::select! {
                biased;
                _ = ctx.shutdown.cancelled() => {
                    info!("tunnel: shutdown requested, stopping the QUIC accept loop");
                    return Ok(());
                }
                incoming = endpoint.accept() => {
                    let Some(incoming) = incoming else {
                        // The endpoint is gone — nothing left to accept.
                        return Ok(());
                    };
                    let app = ctx.app.clone();
                    tokio::spawn(async move {
                        match incoming.await {
                            Ok(connection) => {
                                if let Err(e) = handle_quic_client(app, connection).await {
                                    warn!("tunnel: QUIC client error: {}", e);
                                }
                            }
                            Err(e) => {
                                warn!("tunnel: QUIC handshake failed: {}", e);
                            }
                        }
                    });
                }
            }
        }
    }
}

/// Authenticate one QUIC connection and register it as the tun's
/// session.
async fn handle_quic_client(app: Arc<App>, connection: quinn::Connection) -> anyhow::Result<()> {
    // The tun's control stream is the only stream the tun opens.
    let (send, recv) = tokio::time::timeout(
        std::time::Duration::from_secs(AUTH_TIMEOUT_SECS),
        connection.accept_bi(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("the tun did not open a control stream in time"))?
    .map_err(|e| anyhow::anyhow!("accept the control stream: {e}"))?;
    let mut control = QuicBiStream::new(send, recv);

    let line = tokio::time::timeout(
        std::time::Duration::from_secs(AUTH_TIMEOUT_SECS),
        read_crlf_line(&mut control),
    )
    .await
    .map_err(|_| anyhow::anyhow!("the tun did not send an AUTH line in time"))?
    .map_err(|e| anyhow::anyhow!("read the AUTH line: {e}"))?;
    let (token, tun_name, version) =
        decode_auth(line.as_bytes()).map_err(|e| anyhow::anyhow!("bad AUTH line: {e}"))?;

    if version != TUNNEL_PROTOCOL_VERSION {
        let msg = protocol_version_mismatch(TUNNEL_PROTOCOL_VERSION, version);
        warn!("tunnel {}: {}", tun_name, msg);
        let _ = control.write_all(&encode_auth_reply(Err(&msg))).await;
        let _ = control.flush().await;
        connection.close(0u32.into(), msg.as_bytes());
        return Ok(());
    }

    match validate_token(&app, &token, &tun_name).await {
        Ok(()) => {}
        Err(status) => {
            let reason = match status {
                401 => "Unauthorized",
                403 => "Forbidden",
                _ => "Internal Server Error",
            };
            warn!("tunnel auth failed for {}: status {}", tun_name, status);
            let _ = control.write_all(&encode_auth_reply(Err(reason))).await;
            let _ = control.flush().await;
            connection.close(0u32.into(), reason.as_bytes());
            return Ok(());
        }
    }

    let _ = control
        .write_all(&encode_auth_reply(Ok(TUNNEL_PROTOCOL_VERSION)))
        .await;
    let _ = control.flush().await;

    // Register the session. A duplicate tun name is rejected — two
    // tuns must not fight for the same routing slot.
    let transport: Arc<dyn TunnelTransport> =
        Arc::new(QuicServerTransport::new(connection.clone()));
    if !app.register_tun(tun_name.clone(), transport.clone()).await {
        warn!("tunnel: duplicate tun_name '{}' rejected", tun_name);
        connection.close(0u32.into(), b"duplicate tun name");
        return Ok(());
    }

    crate::tunnel::mark_tun_online(&app, &tun_name).await;
    info!("tun {} connected over quic", tun_name);
    app.add_event(pangolin_core::EventType::TunConnected {
        name: tun_name.clone(),
    });

    // The session ends when the QUIC connection closes. Only remove
    // the registry entry if it is still *this* session — a tun that
    // reconnected on the other transport must not be unregistered by
    // the old session's cleanup.
    connection.closed().await;
    if app.unregister_tun_if(&tun_name, &transport).await {
        crate::tunnel::mark_tun_offline(&app, &tun_name).await;
        info!("tun {} disconnected", tun_name);
        app.add_event(pangolin_core::EventType::TunDisconnected {
            name: tun_name.clone(),
        });
    }
    Ok(())
}
