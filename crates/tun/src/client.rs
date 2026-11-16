//! Pangolin tunnel node (tun) — a **byte relay**.
//!
//! ## Architecture (issue #153 — the tunnel is a splice)
//!
//! The gateway's pingora writes the client's request bytes into a
//! tunnel stream and reads the backend's bytes out of it. The tun's
//! job is therefore tiny:
//!
//! 1. read the `CONNECT <host>:<port>` instruction;
//! 2. resolve the host **in the operator's own network** and dial it;
//! 3. answer `OK` (or `ERR <reason>`);
//! 4. copy bytes in both directions until both sides are done.
//!
//! The tun does not speak HTTP. It never parses a request, never
//! builds one and never buffers a response — which is why SSE,
//! WebSocket upgrades and any other protocol feature the gateway
//! owns work over the tunnel without a counterpart on this side.
//!
//! ## Transports
//!
//! Two transports implement the same interface
//! (`pangolin_core::tunnel::TunnelTransport`):
//!
//! * **QUIC** (primary) — one connection per tun, one bidirectional
//!   stream per request. Encrypted by default. The tun authenticates
//!   on a control stream right after the handshake.
//! * **WebSocket over TCP** (fallback) — the pre-#153 transport, kept
//!   for networks where UDP is blocked.
//!
//! The transport is chosen **once per session**. At the start of a
//! session the tun attempts QUIC with a five-second handshake
//! timeout; on failure it falls back to the WebSocket/TCP transport
//! and stays on it for the rest of the session. It records the time of
//! the failure and does not probe QUIC again for ten minutes, so a
//! network that blocks UDP does not pay the timeout on every
//! reconnect.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use log::{debug, info, warn};

use pangolin_core::tunnel::WsRole;
use pangolin_core::tunnel::pin::PinStore;
use pangolin_core::tunnel::quic::QuicClientTransport;
use pangolin_core::tunnel::{
    TUNNEL_PROTOCOL_VERSION, TUNNEL_VERSION_HEADER, TunnelRole, TunnelStream, TunnelTransport,
    build_ws_upgrade_request, decode_auth_reply, decode_connect, encode_auth, encode_connect_reply,
    read_crlf_line, read_ws_accept_response, relay_bidirectional, tunnel_over_websocket,
};

use fastwebsockets::WebSocket;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::sleep;

use crate::config::{TransportSelector, TunConfig};

/// How long the tun waits for the QUIC handshake before it gives up
/// and falls back to the WebSocket/TCP transport.
///
/// A network that silently drops UDP (no ICMP) never produces an
/// error — the timeout is what saves the connection, so it has to be
/// short enough to keep the fallback responsive and long enough for a
/// slow link to complete the handshake.
pub const QUIC_HANDSHAKE_TIMEOUT_SECS: u64 = 5;

/// How long the tun skips the QUIC probe after a failed attempt.
///
/// Without this, every reconnect in a UDP-blocked network would pay
/// the handshake timeout again.
pub const QUIC_PROBE_SKIP_SECS: i64 = 600;

/// Largest connection instruction the tun accepts.
const MAX_INSTRUCTION_LEN: usize = 4096;

/// How long the tun waits for the gateway's answer to its `AUTH`
/// line.
///
/// A gateway that accepts the QUIC connection but never answers would
/// otherwise hang the tun's session forever.
const AUTH_REPLY_TIMEOUT_SECS: u64 = 10;

// ─────────────────────────────────────────────────────────────────
//  WS handshake helpers (kept from the original file — used by
//  the connect path, not by the per-stream dispatch).
// ─────────────────────────────────────────────────────────────────

/// Outcome of a single connect + session cycle.
enum SessionOutcome {
    /// The session was established and ended cleanly.
    Ended,
    /// The session was established and ended with an error.
    Errored(String),
    /// The session could not be established at all.
    NotConnected(String),
}

pub struct TunnelClient {
    config: TunConfig,
    /// The trust-on-first-use pin file — next to the configuration.
    pin_path: PathBuf,
    /// The record of the last failed QUIC attempt — next to the
    /// configuration, so it survives a restart.
    quic_failure_path: PathBuf,
}

impl TunnelClient {
    /// Build the client. `config_path` is the path of the tun's
    /// configuration file; the pin file and the QUIC-failure record
    /// live next to it.
    pub fn new(config: TunConfig, config_path: &std::path::Path) -> Self {
        Self {
            pin_path: TunConfig::pin_path(config_path),
            quic_failure_path: TunConfig::quic_failure_path(config_path),
            config,
        }
    }

    pub async fn run(&self) {
        info!(
            "tun {} starting, target {} (transport: {})",
            self.config.name, self.config.server, self.config.transport
        );

        const INITIAL_BACKOFF_SECS: u64 = 1;
        const MAX_BACKOFF_SECS: u64 = 30;
        const MAX_JITTER_MS: u64 = 500;

        let mut backoff_secs: u64 = INITIAL_BACKOFF_SECS;

        loop {
            let session_outcome = self.connect_and_handle().await;
            match session_outcome {
                SessionOutcome::Ended => {
                    info!("tun {} disconnected, will reconnect", self.config.name);
                    backoff_secs = INITIAL_BACKOFF_SECS;
                }
                SessionOutcome::Errored(e) => {
                    warn!(
                        "tun {} session errored ({}), will reconnect",
                        self.config.name, e
                    );
                    backoff_secs = INITIAL_BACKOFF_SECS;
                }
                SessionOutcome::NotConnected(e) => {
                    log::error!(
                        "tun {} connect error: {}, reconnecting in {}s",
                        self.config.name,
                        e,
                        backoff_secs
                    );
                }
            }

            let jitter_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_millis() as u64)
                .unwrap_or(0)
                % (MAX_JITTER_MS + 1);
            sleep(Duration::from_millis(backoff_secs * 1000 + jitter_ms)).await;
            backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
        }
    }

    /// Establish one session and serve it until it ends.
    async fn connect_and_handle(&self) -> SessionOutcome {
        let transport = match self.config.transport {
            TransportSelector::Tcp => match self.connect_tcp().await {
                Ok(t) => t,
                Err(e) => return SessionOutcome::NotConnected(e),
            },
            TransportSelector::Quic => match self.connect_quic().await {
                Ok(t) => t,
                Err(e) => return SessionOutcome::NotConnected(e),
            },
            TransportSelector::Auto => {
                if self.quic_recently_failed() {
                    info!(
                        "tun {}: skipping the QUIC probe (the last attempt failed less than {}s ago)",
                        self.config.name, QUIC_PROBE_SKIP_SECS
                    );
                    match self.connect_tcp().await {
                        Ok(t) => t,
                        Err(e) => return SessionOutcome::NotConnected(e),
                    }
                } else {
                    match self.connect_quic().await {
                        Ok(t) => t,
                        Err(e) => {
                            warn!(
                                "tun {}: the QUIC attempt failed ({}); falling back to the \
                                 WebSocket/TCP transport",
                                self.config.name, e
                            );
                            self.record_quic_failure();
                            match self.connect_tcp().await {
                                Ok(t) => t,
                                Err(e) => return SessionOutcome::NotConnected(e),
                            }
                        }
                    }
                }
            }
        };

        match serve_tun_session(&self.config, transport).await {
            Ok(()) => SessionOutcome::Ended,
            Err(e) => SessionOutcome::Errored(e.to_string()),
        }
    }

    /// Connect over QUIC and authenticate on a control stream.
    async fn connect_quic(&self) -> Result<Arc<dyn TunnelTransport>, String> {
        let (addr, server_name) = resolve_server(&self.config.server).await?;
        let pins = PinStore::open(&self.pin_path).map_err(|e| e.to_string())?;

        let (transport, _pinned) = tokio::time::timeout(
            Duration::from_secs(QUIC_HANDSHAKE_TIMEOUT_SECS),
            QuicClientTransport::connect(addr, &server_name, &pins),
        )
        .await
        .map_err(|_| {
            format!(
                "the QUIC handshake to {} timed out after {QUIC_HANDSHAKE_TIMEOUT_SECS}s",
                self.config.server
            )
        })?
        .map_err(|e| e.to_string())?;

        // QUIC has no upgrade request to carry the token in, so the
        // tun opens a control stream and authenticates there.
        let mut control = transport
            .open_stream()
            .await
            .map_err(|e| format!("open the QUIC control stream: {e}"))?;
        control
            .write_all(&encode_auth(
                &self.config.token,
                &self.config.name,
                TUNNEL_PROTOCOL_VERSION,
            ))
            .await
            .map_err(|e| format!("write the AUTH line: {e}"))?;
        control
            .flush()
            .await
            .map_err(|e| format!("flush the AUTH line: {e}"))?;
        let reply = tokio::time::timeout(
            Duration::from_secs(AUTH_REPLY_TIMEOUT_SECS),
            read_crlf_line(&mut control, MAX_INSTRUCTION_LEN),
        )
        .await
        .map_err(|_| {
            format!("the gateway did not answer the AUTH line within {AUTH_REPLY_TIMEOUT_SECS}s")
        })?
        .map_err(|e| format!("read the AUTH reply: {e}"))?;
        match decode_auth_reply(reply.as_bytes()).map_err(|e| e.to_string())? {
            Ok(version) => {
                if version != TUNNEL_PROTOCOL_VERSION {
                    return Err(pangolin_core::tunnel::protocol_version_mismatch(
                        TUNNEL_PROTOCOL_VERSION,
                        version,
                    ));
                }
            }
            Err(reason) => return Err(format!("the gateway refused the tun: {reason}")),
        }

        info!(
            "tun {} connected to ngx {} over quic",
            self.config.name, self.config.server
        );
        Ok(Arc::new(transport))
    }

    /// Connect over the WebSocket/TCP fallback transport.
    async fn connect_tcp(&self) -> Result<Arc<dyn TunnelTransport>, String> {
        let tcp = TcpStream::connect(&self.config.server)
            .await
            .map_err(|e| format!("tcp connect to {}: {e}", self.config.server))?;
        let (mut tcp_read, mut tcp_write) = tcp.into_split();
        let path = format!("/tunnel?name={}", self.config.name);
        let host = &self.config.server;
        // The protocol version rides in the upgrade request — it is
        // the only handshake the TCP fallback has.
        let extra_headers = vec![(
            TUNNEL_VERSION_HEADER.to_string(),
            TUNNEL_PROTOCOL_VERSION.to_string(),
        )];
        let (req_bytes, key) =
            build_ws_upgrade_request(&path, host, &self.config.token, &extra_headers);
        tcp_write
            .write_all(&req_bytes)
            .await
            .map_err(|e| format!("write the upgrade request: {e}"))?;
        tcp_write
            .flush()
            .await
            .map_err(|e| format!("flush the upgrade request: {e}"))?;
        read_ws_accept_response(&mut tcp_read, &key)
            .await
            .map_err(|e| format!("the gateway refused the upgrade: {e}"))?;
        let ws = WebSocket::after_handshake(
            OwnedTcpHalf {
                reader: tcp_read,
                writer: tcp_write,
            },
            WsRole::Client,
        );
        let tunnel = tunnel_over_websocket(ws, TunnelRole::Client);
        info!(
            "tun {} connected to ngx {} over the WebSocket/TCP transport",
            self.config.name, self.config.server
        );
        Ok(Arc::new(tunnel))
    }

    /// Whether the last QUIC attempt failed recently enough that the
    /// probe should be skipped.
    fn quic_recently_failed(&self) -> bool {
        let Ok(text) = std::fs::read_to_string(&self.quic_failure_path) else {
            return false;
        };
        let Ok(failed_at) = text.trim().parse::<i64>() else {
            return false;
        };
        let now = chrono::Utc::now().timestamp();
        now - failed_at < QUIC_PROBE_SKIP_SECS
    }

    /// Record the time of the failed QUIC attempt so the next session
    /// goes straight to the fallback.
    fn record_quic_failure(&self) {
        if let Err(e) = std::fs::write(
            &self.quic_failure_path,
            chrono::Utc::now().timestamp().to_string(),
        ) {
            warn!(
                "tun {}: could not record the QUIC failure in {}: {}",
                self.config.name,
                self.quic_failure_path.display(),
                e
            );
        }
    }
}

/// Resolve `host:port` into a socket address and the name to present
/// in the TLS handshake.
///
/// The host may be a name or an IP literal. Either way it is the
/// operator's gateway address, and the certificate is pinned by
/// fingerprint — so the name is only used for the TLS handshake
/// itself, never for verification.
async fn resolve_server(server: &str) -> Result<(SocketAddr, String), String> {
    let (host, port) = server
        .rsplit_once(':')
        .ok_or_else(|| format!("the server address {server:?} has no port"))?;
    let port: u16 = port
        .parse()
        .map_err(|_| format!("the server address {server:?} has a bad port"))?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let server_name = host.to_string();
    let addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("resolve the server address {server:?}: {e}"))?;
    let addr = addrs
        .into_iter()
        .next()
        .ok_or_else(|| format!("the server address {server:?} did not resolve"))?;
    Ok((addr, server_name))
}

struct OwnedTcpHalf {
    reader: tokio::net::tcp::OwnedReadHalf,
    writer: tokio::net::tcp::OwnedWriteHalf,
}

impl AsyncRead for OwnedTcpHalf {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.reader).poll_read(cx, buf)
    }
}

impl AsyncWrite for OwnedTcpHalf {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.writer).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.writer).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.writer).poll_shutdown(cx)
    }
}

/// Run the tun-side accept loop: every stream the gateway opens is a
/// request the tun has to relay to a backend.
async fn serve_tun_session(config: &TunConfig, transport: Arc<dyn TunnelTransport>) -> Result<()> {
    let name = config.name.clone();
    loop {
        let stream = match transport.accept_stream().await {
            Some(Ok(s)) => s,
            Some(Err(e)) => {
                warn!("tun {} accept error: {}", name, e);
                return Err(anyhow::anyhow!("tun {name} accept error: {e}"));
            }
            None => {
                info!("tun {} session ended", name);
                return Ok(());
            }
        };

        let name_for_log = name.clone();
        tokio::spawn(async move {
            if let Err(e) = relay_stream(stream, &name_for_log).await {
                warn!("tun {} relay error: {}", name_for_log, e);
            }
        });
    }
}

// ─────────────────────────────────────────────────────────────────
//  The relay
// ─────────────────────────────────────────────────────────────────

/// Relay one tunnel stream to a backend.
///
/// The stream starts with a single `CONNECT <host>:<port>` line; the
/// answer is `OK` or `ERR <reason>`. After `OK` the stream carries
/// nothing but bytes.
async fn relay_stream(mut stream: TunnelStream, name: &str) -> Result<()> {
    let line = read_crlf_line(&mut stream, MAX_INSTRUCTION_LEN)
        .await
        .map_err(|e| anyhow::anyhow!("read the CONNECT instruction: {e}"))?;
    let (host, port) = decode_connect(line.as_bytes())
        .map_err(|e| anyhow::anyhow!("bad CONNECT instruction: {e}"))?;

    let backend = match dial_target(&host, port).await {
        Ok(backend) => backend,
        Err(reason) => {
            // The gateway turns `ERR` into a 502 for the client.
            stream
                .write_all(&encode_connect_reply(Err(&reason)))
                .await
                .map_err(|e| anyhow::anyhow!("write the ERR reply: {e}"))?;
            stream
                .flush()
                .await
                .map_err(|e| anyhow::anyhow!("flush the ERR reply: {e}"))?;
            return Ok(());
        }
    };

    stream
        .write_all(&encode_connect_reply(Ok(())))
        .await
        .map_err(|e| anyhow::anyhow!("write the OK reply: {e}"))?;
    stream
        .flush()
        .await
        .map_err(|e| anyhow::anyhow!("flush the OK reply: {e}"))?;

    let mut backend = backend;
    info!("tun {} relaying to {}:{}", name, host, port);
    relay_bidirectional(&mut stream, &mut backend)
        .await
        .map_err(|e| anyhow::anyhow!("relay to {host}:{port}: {e}"))?;
    Ok(())
}

/// Resolve the target and dial it.
///
/// The host is resolved **here**, in the operator's own network — a
/// backend that is only reachable from the tun's side is reachable
/// through the tunnel.
///
/// Every address the resolver returns is tried, not just the first:
/// `lookup_host` ordering is OS-dependent (Linux lists `::1` before
/// `127.0.0.1` for `localhost`), and the back-half of the connection
/// failures are "wrong address family" — the backend only listens on
/// v4 but the resolver hands us v6 first.
async fn dial_target(host: &str, port: u16) -> Result<TcpStream, String> {
    let addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("DNS lookup failed for {host}: {e}"))?;
    let mut last_err: Option<std::io::Error> = None;
    for addr in addrs {
        match TcpStream::connect(addr).await {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                debug!(
                    "tun: connect to {} failed: {} — trying the next address",
                    addr, e
                );
                last_err = Some(e);
            }
        }
    }
    Err(format!(
        "no reachable address for {host}:{port}: {}",
        last_err
            .map(|e| e.to_string())
            .unwrap_or_else(|| "no addresses".into())
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The server address is split into the socket address and the
    /// name presented in the TLS handshake.
    #[tokio::test]
    async fn server_address_is_split() {
        let (addr, name) = resolve_server("127.0.0.1:9001").await.unwrap();
        assert_eq!(addr, "127.0.0.1:9001".parse::<SocketAddr>().unwrap());
        assert_eq!(name, "127.0.0.1");
    }

    /// A host name works too — the certificate is pinned by
    /// fingerprint, so the name is only used for the handshake.
    #[tokio::test]
    async fn server_address_accepts_a_host_name() {
        let (addr, name) = resolve_server("localhost:9001").await.unwrap();
        assert_eq!(addr.port(), 9001);
        assert_eq!(name, "localhost");
    }

    /// An address without a port is refused up front rather than at
    /// the first connect.
    #[tokio::test]
    async fn server_address_without_port_is_refused() {
        assert!(resolve_server("gateway.example.com").await.is_err());
        assert!(
            resolve_server("gateway.example.com:not-a-port")
                .await
                .is_err()
        );
    }

    fn client_for(dir: &std::path::Path) -> TunnelClient {
        let config = TunConfig {
            server: "127.0.0.1:9001".into(),
            token: "t".into(),
            name: "office".into(),
            ..TunConfig::default()
        };
        let config_path = dir.join("tun.yml");
        TunnelClient::new(config, &config_path)
    }

    /// A tun that has never tried QUIC probes it on the next session.
    #[test]
    fn a_fresh_tun_probes_quic() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(dir.path());
        assert!(
            !client.quic_recently_failed(),
            "a tun that never tried QUIC must probe it"
        );
    }

    /// After a failed QUIC attempt the tun skips the probe — and the
    /// record survives a restart, so a tun that is restarted every
    /// few minutes does not pay the handshake timeout every time.
    #[test]
    fn a_failed_quic_attempt_skips_the_probe_across_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(dir.path());
        client.record_quic_failure();
        assert!(client.quic_recently_failed());

        // A "restart" — a new client reading the same directory.
        let restarted = client_for(dir.path());
        assert!(
            restarted.quic_recently_failed(),
            "the QUIC failure record must survive a restart"
        );
    }

    /// The skip expires: after the ten-minute window the tun probes
    /// QUIC again, so a network that unblocks UDP is picked up
    /// without operator action.
    #[test]
    fn the_quic_probe_skip_expires() {
        let dir = tempfile::tempdir().unwrap();
        let client = client_for(dir.path());
        client.record_quic_failure();
        // Backdate the record past the skip window.
        let old = chrono::Utc::now().timestamp() - QUIC_PROBE_SKIP_SECS - 60;
        std::fs::write(client.quic_failure_path.clone(), old.to_string()).unwrap();
        assert!(
            !client.quic_recently_failed(),
            "the tun must probe QUIC again after the skip window"
        );
    }
}
