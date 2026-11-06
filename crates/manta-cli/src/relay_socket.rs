//! MAN-320 (D11): the UDP socket a `[[wsjtx_relay]]` listens on, bound in
//! `manta-cli` and handed to `manta_server::relay::wsjtx::run`, so
//! `manta-server` gains no dependency edge. An IPv4 multicast group binds
//! `0.0.0.0:port` and joins the group; a unicast address is a plain bind,
//! so a taken port is a clear startup error rather than a silently shared
//! one.

use anyhow::{anyhow, Result};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};

/// Binds `addr` for a WSJT-X relay, nonblocking (tokio adopts it). The
/// error is the whole operator-facing message (devex 3).
pub(crate) fn bind_wsjtx_socket(addr: SocketAddr) -> Result<UdpSocket> {
    bind(addr).map_err(|e| {
        anyhow!(
            "[[wsjtx_relay]] cannot listen on {addr}: {e} -- another program may own the port; \
             set WSJT-X's UDP Server to a multicast group (e.g. 224.0.0.1) and listen there"
        )
    })
}

fn bind(addr: SocketAddr) -> std::io::Result<UdpSocket> {
    let socket = match addr {
        SocketAddr::V4(v4) if v4.ip().is_multicast() => {
            let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
            s.set_reuse_address(true)?;
            s.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, v4.port()).into())?;
            s.join_multicast_v4(v4.ip(), &Ipv4Addr::UNSPECIFIED)?;
            UdpSocket::from(s)
        }
        _ => UdpSocket::bind(addr)?,
    };
    socket.set_nonblocking(true)?;
    Ok(socket)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_wsjtx_socket_binds_a_unicast_address() {
        let s = bind_wsjtx_socket("127.0.0.1:0".parse().unwrap()).unwrap();
        let local = s.local_addr().unwrap();
        assert_eq!(local.ip(), std::net::IpAddr::from([127, 0, 0, 1]));
        assert_ne!(local.port(), 0);
        // Nonblocking: an empty socket reports WouldBlock instead of waiting.
        let mut buf = [0u8; 16];
        let e = s.recv_from(&mut buf).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::WouldBlock);
    }

    #[cfg(unix)]
    #[test]
    fn bind_wsjtx_socket_joins_an_ipv4_multicast_group() {
        let s = bind_wsjtx_socket("224.0.0.1:0".parse().unwrap()).unwrap();
        let local = s.local_addr().unwrap();
        assert_eq!(local.ip(), std::net::IpAddr::from([0, 0, 0, 0]));
        assert_ne!(local.port(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn bind_wsjtx_socket_shares_a_multicast_port_with_a_reuse_enabled_peer() {
        let peer = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )
        .unwrap();
        peer.set_reuse_address(true).unwrap();
        peer.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into())
            .unwrap();
        let peer_addr = peer.local_addr().unwrap().as_socket_ipv4().unwrap();

        let relay = bind_wsjtx_socket(
            SocketAddrV4::new(Ipv4Addr::new(224, 0, 0, 1), peer_addr.port()).into(),
        )
        .unwrap();
        assert_eq!(relay.local_addr().unwrap(), SocketAddr::V4(peer_addr));
        // Inspect the flag rather than waiting for an empty multicast receive.
        assert!(socket2::SockRef::from(&relay).nonblocking().unwrap());
        // The peer still owns its binding after the relay has joined it.
        assert_eq!(
            peer.local_addr().unwrap().as_socket_ipv4().unwrap(),
            peer_addr
        );
    }

    #[test]
    fn a_taken_unicast_port_names_the_relay() {
        let first = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = first.local_addr().unwrap();
        let err = format!("{:#}", bind_wsjtx_socket(addr).unwrap_err());
        assert!(
            err.starts_with(&format!("[[wsjtx_relay]] cannot listen on {addr}: ")),
            "{err}"
        );
        assert!(
            err.ends_with(
                " -- another program may own the port; set WSJT-X's UDP Server to a multicast \
                 group (e.g. 224.0.0.1) and listen there"
            ),
            "{err}"
        );
    }
}
