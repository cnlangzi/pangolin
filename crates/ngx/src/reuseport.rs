//! The listeners the gateway binds outside pingora.
//!
//! pingora's own listeners take a `TcpSocketOptions`, but the tunnel
//! listener and the QUIC endpoint are plain sockets this crate binds
//! itself. Both set `SO_REUSEPORT` **before** the bind.
//!
//! ## Why
//!
//! Two generations of the gateway have to be able to hold the same
//! port at the same time (issue #233 lays the groundwork for the
//! handover). The kernel spreads the incoming connections over the
//! sockets of a reuseport group by the hash of the connection's
//! 4-tuple, so the outgoing generation can drain while the incoming
//! one already serves — with no window in which nobody holds the
//! port.
//!
//! The option is a property of the *socket*, and the kernel only
//! lets the second socket of the group bind when **both** asked for
//! it. A listener that binds without it takes the port away from the
//! next process that tries: the second generation would fail with
//! `EADDRINUSE` and the handover would not happen at all.
//!
//! The management API's listener lives in the `api` crate and sets
//! the option itself — the API is deliberately independent of the
//! gateway's internals (its own port, its own authentication), so it
//! does not share this module.

use std::net::SocketAddr;

use socket2::{Domain, Protocol, SockAddr, Socket, Type};

/// The backlog the gateway's own listeners use.
///
/// The number tokio's `TcpListener::bind` uses — the behaviour these
/// listeners had before their socket moved into this module.
const BACKLOG: i32 = 1024;

/// Bind a TCP listener to `addr` with `SO_REUSEPORT` set.
///
/// The socket comes back **non-blocking** and listening, ready to be
/// handed to tokio.
pub fn bind_tcp(addr: SocketAddr) -> std::io::Result<tokio::net::TcpListener> {
    let socket = socket(addr, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_nonblocking(true)?;
    socket.bind(&sockaddr(addr))?;
    socket.listen(BACKLOG)?;
    tokio::net::TcpListener::from_std(socket.into())
}

/// Bind a UDP socket to `addr` with `SO_REUSEPORT` set.
///
/// The socket stays **blocking**: quinn takes the raw socket over
/// and configures it itself (`pangolin_tun::bind_endpoint_with_socket`).
pub fn bind_udp(addr: SocketAddr) -> std::io::Result<std::net::UdpSocket> {
    let socket = socket(addr, Type::DGRAM, Some(Protocol::UDP))?;
    socket.bind(&sockaddr(addr))?;
    Ok(socket.into())
}

/// A fresh socket of `ty` with `SO_REUSEPORT` already set.
fn socket(addr: SocketAddr, ty: Type, protocol: Option<Protocol>) -> std::io::Result<Socket> {
    let socket = Socket::new(Domain::for_address(addr), ty, protocol)?;
    // The option has to be set **before** the bind — see the module
    // doc.
    //
    // `SO_REUSEPORT` is a Unix option, and the `socket2` crate
    // exposes it on the Unix platforms that have it. Windows has no
    // equivalent that spreads the connections over the sockets of the
    // group — its `SO_REUSEADDR` lets a second socket bind the
    // address, but the kernel hands every connection to one of them.
    // The handover this option is for is a Linux feature, so on the
    // other platforms the listener stays a plain one.
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    Ok(socket)
}

/// The socket's own address, in the form `bind` wants it.
fn sockaddr(addr: SocketAddr) -> SockAddr {
    addr.into()
}

// The tests below are about the option itself, and `SO_REUSEPORT` is
// a Unix option — see `socket` above.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// The loopback address on `port`.
    fn loopback(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    /// Two listeners can hold the same port at the same time.
    ///
    /// This is the whole point of `SO_REUSEPORT` (issue #233): the
    /// second generation of the gateway binds the port the first one
    /// still holds, and the kernel spreads the connections over the
    /// two sockets.
    ///
    /// Without the option the second bind fails with `EADDRINUSE`,
    /// which is exactly what the handover cannot afford.
    ///
    /// The test runs inside a tokio runtime: handing the socket to
    /// tokio needs a reactor.
    #[tokio::test]
    async fn two_listeners_can_hold_the_same_port() {
        let first = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a probe socket");
        let port = first.local_addr().expect("the probe's address").port();
        drop(first);

        let a = bind_tcp(loopback(port)).expect("the first listener binds");
        let b = bind_tcp(loopback(port)).expect("the second listener binds the same port");

        assert_eq!(a.local_addr().expect("address").port(), port);
        assert_eq!(b.local_addr().expect("address").port(), port);
    }

    /// The same holds for the UDP socket the QUIC endpoint needs.
    ///
    /// The QUIC endpoint shares its port number with the tunnel's TCP
    /// listener, and the endpoint is built on a socket this module
    /// binds — so the option has to be on the UDP socket too.
    #[tokio::test]
    async fn two_udp_sockets_can_hold_the_same_port() {
        let probe = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind a probe socket");
        let port = probe.local_addr().expect("the probe's address").port();
        drop(probe);

        let a = bind_udp(loopback(port)).expect("the first socket binds");
        let b = bind_udp(loopback(port)).expect("the second socket binds the same port");

        assert_eq!(a.local_addr().expect("address").port(), port);
        assert_eq!(b.local_addr().expect("address").port(), port);
    }

    /// A listener that does **not** ask for `SO_REUSEPORT` takes the
    /// port away from the next one.
    ///
    /// This is the failure the option exists to prevent: the second
    /// generation of the gateway would fail to bind and exit.
    #[tokio::test]
    async fn a_listener_without_the_option_keeps_the_port_to_itself() {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a probe socket");
        let port = probe.local_addr().expect("the probe's address").port();
        drop(probe);

        let _plain = std::net::TcpListener::bind(loopback(port)).expect("the plain listener binds");
        let second = bind_tcp(loopback(port));
        assert!(
            second.is_err(),
            "the reuseport listener must not be able to join a group that did not ask for it"
        );
    }
}
