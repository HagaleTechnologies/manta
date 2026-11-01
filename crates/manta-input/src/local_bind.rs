//! Local UDP bind addresses chosen from the destination's address family and scope.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, UdpSocket};

/// Choose localhost for loopback peers; keep wildcard binds for other peers.
pub fn udp_local_addr(peer: IpAddr) -> IpAddr {
    match peer {
        IpAddr::V4(ip) => IpAddr::V4(if ip.is_loopback() {
            Ipv4Addr::LOCALHOST
        } else {
            Ipv4Addr::UNSPECIFIED
        }),
        IpAddr::V6(ip) => IpAddr::V6(if ip.is_loopback() {
            Ipv6Addr::LOCALHOST
        } else {
            Ipv6Addr::UNSPECIFIED
        }),
    }
}

/// Open an ephemeral UDP socket with a local address chosen for `peer`.
pub fn bind_udp_for(peer: IpAddr) -> io::Result<UdpSocket> {
    UdpSocket::bind((udp_local_addr(peer), 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_peers_bind_loopback_of_their_family() {
        for (peer, want) in [
            ("127.0.0.1", "127.0.0.1"),
            ("127.0.0.99", "127.0.0.1"),
            ("::1", "::1"),
        ] {
            let got = bind_udp_for(peer.parse().unwrap()).unwrap();
            assert_eq!(
                got.local_addr().unwrap().ip(),
                want.parse::<IpAddr>().unwrap()
            );
        }
    }

    #[test]
    fn lan_peers_keep_today_s_wildcard_bind() {
        for (peer, want) in [
            ("192.168.1.50", "0.0.0.0"),
            ("239.193.82.12", "0.0.0.0"),
            ("2001:db8::1", "::"),
        ] {
            let got = bind_udp_for(peer.parse().unwrap()).unwrap();
            assert_eq!(
                got.local_addr().unwrap().ip(),
                want.parse::<IpAddr>().unwrap()
            );
        }
    }

    #[test]
    fn udp_local_addr_is_the_pure_mapping_the_call_sites_use() {
        for (peer, want) in [
            ("127.0.0.1", "127.0.0.1"),
            ("127.0.0.99", "127.0.0.1"),
            ("::1", "::1"),
            ("192.168.1.50", "0.0.0.0"),
            ("239.193.82.12", "0.0.0.0"),
            ("2001:db8::1", "::"),
        ] {
            assert_eq!(
                udp_local_addr(peer.parse().unwrap()),
                want.parse::<IpAddr>().unwrap()
            );
        }
    }
}
