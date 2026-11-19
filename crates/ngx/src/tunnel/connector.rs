//! The gateway's tunnel delivery: a pingora L4 connector that hands
//! pingora a tunnel **byte stream**.
//!
//! ## Why a connector and not an executor
//!
//! Before issue #153 the gateway encoded the request into a
//! `TunnelHttpFrame`, wrote it to a yamux stream and waited for a
//! fully buffered `HttpResponse` to come back. A buffered response
//! cannot represent an endless stream, so SSE and WebSocket upgrades
//! each got their own hand-written byte-relay path on the tunnel side.
//!
//! Now the tunnel is a **splice**: pingora writes the client's request
//! bytes into a tunnel stream and reads the backend's bytes out of
//! it. The gateway's own HTTP stack therefore serves every backend —
//! direct or tunnelled — through one path, and the tun stops speaking
//! HTTP entirely.
//!
//! ## The connection instruction
//!
//! Every stream starts with a single line the gateway writes and the
//! tun answers:
//!
//! ```text
//! gateway → tun:  CONNECT <host>:<port>\r\n
//! tun → gateway:  OK\r\n          (or)   ERR <reason>\r\n
//! ```
//!
//! The host travels as a **name**, so the tun resolves it in the
//! operator's own network: a backend that is only reachable from the
//! tun's side is reachable through the tunnel.
//!
//! ## Why a loopback socket
//!
//! The L4 seam hands the connector's return value to the rest of
//! pingora, which identifies a connection by its **file descriptor**
//! (`Stream::id()` → `getpeername`). A tunnel stream has no file
//! descriptor — it is a yamux stream inside a WebSocket, or a QUIC
//! bidirectional stream. So the tunnel stream is bridged onto a
//! loopback socket pair and pingora gets the socket end.
//!
//! That buys the whole socket machinery: the fd check, the keep-alive
//! handling and the connection pool all behave exactly as they do for
//! a TCP backend, and a pooled tunnel stream can be reused for the
//! next request to the same backend. It is also why the peer carries
//! a `group_key` — see [`tunnel_group_key`].

use std::sync::Arc;

use async_trait::async_trait;
use log::{debug, warn};
// The `pingora` crate re-exports `pingora-core` (`pub use
// pingora_core::*`), so `pingora::…` and `pingora_core::…` name the
// same crate and the same types. The L4 seam is named through
// `pingora::` because that is the crate the gateway depends on
// directly — the L4 types it needs (`L4Connect`, `Stream`) are
// re-exported from there.
use pingora::connectors::L4Connect;
use pingora::prelude::*;
use pingora::protocols::l4::stream::Stream;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use pangolin_tun::{
    TunnelStream, TunnelTransport, decode_connect_reply, encode_connect, read_crlf_line,
};

use crate::App;

/// A pingora L4 connector that opens a tunnel byte stream.
///
/// It is installed on the tunnel peer's `PeerOptions::custom_l4`, so
/// pingora's own HTTP client writes the request into the stream and
/// reads the response out of it. The gateway never re-creates the
/// request — the bytes the client sent are the bytes the backend
/// receives (modulo the gateway's own proxy policy).
#[derive(Clone)]
pub struct TunnelConnector {
    app: Arc<App>,
    tun_name: String,
    host: String,
    port: u16,
}

impl std::fmt::Debug for TunnelConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TunnelConnector")
            .field("tun", &self.tun_name)
            .field("target", &format!("{}:{}", self.host, self.port))
            .finish()
    }
}

impl TunnelConnector {
    /// Build a connector for one (tun, backend) pair.
    pub fn new(app: Arc<App>, tun_name: &str, host: &str, port: u16) -> Self {
        Self {
            app,
            tun_name: tun_name.to_string(),
            host: host.to_string(),
            port,
        }
    }
}

#[async_trait]
impl L4Connect for TunnelConnector {
    async fn connect(&self, _addr: &pingora::protocols::l4::socket::SocketAddr) -> Result<Stream> {
        // The session is looked up per request, not per peer: a tun
        // may come and go while the gateway is running, and a stale
        // handle must not be reused.
        let transport = {
            let sessions = self.app.tun_sessions.read().await;
            sessions.get(&self.tun_name).cloned()
        };
        let Some(transport) = transport else {
            return Err(Error::explain(
                ErrorType::ConnectError,
                format!("tun {} is not online", self.tun_name),
            ));
        };

        // The whole exchange is bounded. The pre-#153 executor
        // bounded the response read at 60 s and answered 504; without
        // a bound here, a wedged tun (or a tun whose DNS lookup
        // hangs) would hold a pingora worker for as long as it liked.
        let exchange = tokio::time::timeout(
            CONNECT_TIMEOUT,
            connect_exchange(transport.as_ref(), &self.host, self.port),
        )
        .await;

        let stream = match exchange {
            Ok(Ok(stream)) => stream,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(Error::explain(
                    ErrorType::ConnectError,
                    format!(
                        "tun {}: the CONNECT exchange with {}:{} did not finish within {}s",
                        self.tun_name,
                        self.host,
                        self.port,
                        CONNECT_TIMEOUT.as_secs()
                    ),
                ));
            }
        };

        debug!(
            "tunnel {}: connected to {}:{} over {}",
            self.tun_name,
            self.host,
            self.port,
            transport.kind()
        );

        // Hand pingora a socket it can identify by its file
        // descriptor, and bridge the tunnel stream onto it.
        let (socket, bridge_end) = mint_loopback_pair().await.map_err(|e| {
            Error::explain(
                ErrorType::ConnectError,
                format!("tun {}: loopback socket: {e}", self.tun_name),
            )
        })?;
        tokio::spawn(bridge_tunnel_to_socket(stream, bridge_end));
        Ok(Stream::from(socket))
    }
}

/// How long the CONNECT exchange may take before the request is
/// failed.
///
/// The exchange is: open a stream, write the `CONNECT` instruction,
/// read the tun's `OK` / `ERR` answer. The tun only answers after it
/// has resolved and dialled the backend, so this bounds the DNS
/// lookup and the connect too.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Open a tunnel stream and run the `CONNECT` exchange on it.
///
/// The tun answers `OK` or `ERR <reason>` before any request byte
/// travels; `ERR` becomes a 502 for the client.
async fn connect_exchange(
    transport: &dyn TunnelTransport,
    host: &str,
    port: u16,
) -> Result<pangolin_tun::TunnelStream> {
    let mut stream = transport
        .open_stream()
        .await
        .map_err(|e| Error::explain(ErrorType::ConnectError, format!("open stream: {e}")))?;

    // The connection instruction: the target by **name**, so the tun
    // resolves it in its own network.
    //
    // The instruction is a **line**, so a host carrying CR, LF or NUL
    // would split it and leave a forged line in the tun's stream. The
    // backend URL is admin-controlled, so this is a defence in depth
    // — but a clear error beats a malformed instruction.
    if host.contains(['\r', '\n', '\0']) {
        return Err(Error::explain(
            ErrorType::ConnectError,
            "the backend host contains an illegal character",
        ));
    }
    stream
        .write_all(&encode_connect(host, port))
        .await
        .map_err(|e| Error::explain(ErrorType::ConnectError, format!("write CONNECT: {e}")))?;
    stream
        .flush()
        .await
        .map_err(|e| Error::explain(ErrorType::ConnectError, format!("flush CONNECT: {e}")))?;

    // The crate bounds the line length itself (4096 bytes), so a peer
    // that never sends a newline cannot make us allocate without
    // limit.
    let reply = read_crlf_line(&mut stream)
        .await
        .map_err(|e| Error::explain(ErrorType::ConnectError, format!("read CONNECT reply: {e}")))?;
    match decode_connect_reply(reply.as_bytes())
        .map_err(|e| Error::explain(ErrorType::ConnectError, format!("bad CONNECT reply: {e}")))?
    {
        Ok(()) => Ok(stream),
        Err(reason) => {
            warn!("tunnel: the backend refused the connection: {reason}");
            Err(Error::explain(
                ErrorType::ConnectError,
                format!("the backend refused the connection: {reason}"),
            ))
        }
    }
}

/// Derive the peer group key for a tunnel backend.
///
/// pingora pools upstream connections per peer, and `HttpPeer::hash`
/// includes `group_key`. Two backends that differ only by host must
/// therefore never share a pooled connection — a stream opened for one
/// backend would otherwise be handed to a request for another. The
/// key is derived from everything that identifies the backend: the
/// tun, the host, the port and the scheme.
///
/// This is the **explicit** separator. The SNI also lands in
/// pingora's peer hash, but the SNI is a TLS concept; relying on it
/// for a plain-HTTP backend would be an accident rather than a
/// contract.
pub fn tunnel_group_key(tun_name: &str, host: &str, port: u16, tls: bool) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    tun_name.hash(&mut hasher);
    host.hash(&mut hasher);
    port.hash(&mut hasher);
    tls.hash(&mut hasher);
    hasher.finish()
}

// ---------------------------------------------------------------------------
// The loopback socket
// ---------------------------------------------------------------------------

/// Mint a connected pair of loopback sockets.
///
/// The two ends are interchangeable: one goes to pingora, the other
/// carries the tunnel stream.
///
/// **Each pair gets its own listener.** The obvious implementation —
/// one shared listener, then `connect` and `accept` as two independent
/// awaits — is a cross-request data leak: `connect` parks the task
/// while it waits, so with two concurrent requests the accept order
/// need not match the connect order and a request can end up with
/// *another* request's accepted socket. One request's bytes would
/// then travel into another request's tunnel stream.
///
/// A fresh listener has room for exactly one pending connection, so
/// the accept is guaranteed to be the one we just made. The listener
/// is dropped as soon as the pair is minted.
async fn mint_loopback_pair() -> std::io::Result<(TcpStream, TcpStream)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let client = TcpStream::connect(addr).await?;
    let (server, _) = listener.accept().await?;
    drop(listener);
    Ok((client, server))
}

/// Bridge the tunnel stream onto the loopback socket pingora writes
/// into.
///
/// The two directions are independent: the request bytes flow from
/// pingora to the tun, the response bytes flow back. Both run
/// concurrently — the request direction does **not** end when pingora
/// has sent everything (pingora keeps the socket open for the
/// response), so pumping them one after the other would deadlock.
///
/// When pingora closes its side of the socket, the tunnel stream is
/// finished with: dropping it resets the yamux/QUIC stream, which
/// makes the tun close its backend connection.
async fn bridge_tunnel_to_socket(tunnel: TunnelStream, socket: TcpStream) {
    let (tunnel_read, tunnel_write) = tokio::io::split(tunnel);
    let (socket_read, socket_write) = tokio::io::split(socket);

    // The response direction: the tun → pingora.
    let response = tokio::spawn(async move {
        copy_half(tunnel_read, socket_write).await;
    });
    // The request direction: pingora → the tun.
    copy_half(socket_read, tunnel_write).await;
    // pingora closed its side — the tunnel stream is done with.
    response.abort();
}

/// Copy one direction until end-of-file, then half-close the writer.
///
/// A half-close (not a full close) matters: the tun shuts the backend
/// socket's **write** side down when the request side ends and keeps
/// reading, so a response that only arrives after the request is
/// complete still reaches the client.
async fn copy_half<R, W>(mut reader: R, mut writer: W)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if writer.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let _ = writer.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two backends that differ only by host must not share a pooled
    /// connection — that is the whole point of the group key.
    #[test]
    fn group_key_separates_backends_that_differ_only_by_host() {
        let a = tunnel_group_key("office", "10.0.0.1", 9000, false);
        let b = tunnel_group_key("office", "10.0.0.2", 9000, false);
        assert_ne!(a, b, "two hosts on the same port must not share a pool");
    }

    /// The same backend must map to the same key, or every request
    /// would open a new connection.
    #[test]
    fn group_key_is_stable_for_the_same_backend() {
        let a = tunnel_group_key("office", "10.0.0.1", 9000, false);
        let b = tunnel_group_key("office", "10.0.0.1", 9000, false);
        assert_eq!(a, b);
    }

    /// The tun and the scheme are part of the identity too.
    #[test]
    fn group_key_separates_tuns_and_schemes() {
        let base = tunnel_group_key("office", "10.0.0.1", 9000, false);
        assert_ne!(base, tunnel_group_key("home", "10.0.0.1", 9000, false));
        assert_ne!(base, tunnel_group_key("office", "10.0.0.1", 9000, true));
        assert_ne!(base, tunnel_group_key("office", "10.0.0.1", 9001, false));
    }

    /// The loopback pair is connected: a byte written to one end
    /// arrives at the other.
    #[tokio::test]
    async fn loopback_pair_is_connected() {
        let (mut a, mut b) = mint_loopback_pair().await.unwrap();
        a.write_all(b"hello").await.unwrap();
        let mut buf = [0u8; 5];
        b.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");
    }

    /// Two concurrent requests must not be able to take each other's
    /// accepted socket.
    ///
    /// The connect and the accept are two independent awaits against
    /// one shared listener, and `connect` parks the task while it
    /// waits — so the accept order need not match the connect order.
    /// When they crossed, one request's bytes would travel into
    /// another request's tunnel stream: a cross-request (and
    /// cross-tenant) data leak.
    ///
    /// The test mints several pairs at once and checks that each pair
    /// is connected **to itself**: the marker one end writes has to
    /// come back on the other end of the same pair.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_pairs_are_not_crossed() {
        let mut tasks = Vec::new();
        for marker in 0u8..8 {
            tasks.push(tokio::spawn(async move {
                let (mut client, mut server) = mint_loopback_pair().await.unwrap();
                client.write_all(&[marker]).await.unwrap();
                let mut buf = [0u8; 1];
                server.read_exact(&mut buf).await.unwrap();
                assert_eq!(
                    buf[0], marker,
                    "the pair was crossed: another request's socket was accepted"
                );
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
    }

    /// The bridge carries the request bytes to the tunnel side and the
    /// response bytes back — the splice itself.
    #[tokio::test]
    async fn bridge_carries_bytes_in_both_directions() {
        let (tunnel_side, mut peer_side) = tokio::io::duplex(1024);
        let tunnel: TunnelStream = Box::new(tunnel_side);
        let (mut socket, bridge_end) = mint_loopback_pair().await.unwrap();
        tokio::spawn(bridge_tunnel_to_socket(tunnel, bridge_end));

        // pingora → tun
        let request = b"GET / HTTP/1.1\r\n\r\n";
        socket.write_all(request).await.unwrap();
        let mut buf = vec![0u8; 64];
        let n = peer_side.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], request);

        // tun → pingora
        peer_side
            .write_all(b"HTTP/1.1 200 OK\r\n\r\n")
            .await
            .unwrap();
        let mut got = Vec::new();
        let mut chunk = [0u8; 64];
        while got.len() < 19 {
            let n = socket.read(&mut chunk).await.unwrap();
            assert!(n > 0, "the bridge must not close early");
            got.extend_from_slice(&chunk[..n]);
        }
        assert_eq!(&got[..19], b"HTTP/1.1 200 OK\r\n\r\n");
    }
}
