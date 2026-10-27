//! MAN-320 (D11): the UDP socket a `[[wsjtx_relay]]` listens on, bound in
//! `manta-cli` and handed to `manta_server::relay::wsjtx::run`, so
//! `manta-server` gains no dependency edge. An IPv4 multicast group binds
//! `0.0.0.0:port` and joins the group; a unicast address is a plain bind,
//! so a taken port is a clear startup error rather than a silently shared
//! one.
//!
//! `socket2` is only a dev-dependency of `manta-cli` and D11 forbids a
//! `Cargo.toml` edit, so this uses `std::net::UdpSocket`, which cannot set
//! `SO_REUSEADDR`/`SO_REUSEPORT` before binding: a multicast listener does
//! not yet share its port with another program bound without reuse. Moving
//! `socket2` to `[dependencies]` (a CODEOWNERS path) restores the ka9q
//! pattern (`manta-input/src/ka9q/mod.rs`).

use anyhow::{anyhow, Result};
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
            let s = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, v4.port()))?;
            s.join_multicast_v4(v4.ip(), &Ipv4Addr::UNSPECIFIED)?;
            s
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
