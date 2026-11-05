//! MAN-142: what `manta run` needs to start `[pskreporter]` and
//! `[[wsjtx_output]]`: the file-replay guard, the sockets (bound before
//! anything is spawned, as `[[wsjtx_relay]]` sockets are) and the startup
//! lines. The workers live in `manta_server::{pskreporter, wsjtx_output}`.

use anyhow::{anyhow, Result};
use manta_server::config::{is_loopback_host, PskReporterConfig, WsjtxOutputConfig};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};

/// The `[pskreporter]`/`[[wsjtx_output]]` tables `start_spot_server` runs
/// (`Default` = none).
#[derive(Debug, Clone, Default)]
pub(crate) struct SpotOutputs {
    /// The `[pskreporter]` table to run: enabled, and allowed on this
    /// source by [`pskreporter_for_run`].
    pub pskreporter: Option<PskReporterConfig>,
    /// A `[pskreporter]` table is configured (enabled or not, replay or
    /// not): its metric families render, at 0 until it sends (D15).
    pub pskreporter_registered: bool,
    /// Every `[[wsjtx_output]]` block, disabled ones included.
    pub wsjtx: Vec<WsjtxOutputConfig>,
    /// Any input is a file: WSJT-X `Decode`s carry `Off air` (D10).
    pub replay: bool,
}

impl SpotOutputs {
    /// The outputs `manta run` starts for `loaded`'s tables on a source
    /// that is (`replay`) or is not a file replay.
    pub fn for_run(
        pskreporter: Option<&PskReporterConfig>,
        wsjtx: &[WsjtxOutputConfig],
        replay: bool,
    ) -> Self {
        Self {
            pskreporter: pskreporter_for_run(pskreporter.filter(|p| p.enabled), replay),
            pskreporter_registered: pskreporter.is_some(),
            wsjtx: wsjtx.to_vec(),
            replay,
        }
    }
}

/// D10: the note a file replay prints instead of reporting to a host that
/// is not this machine.
pub(crate) fn replay_note(host: &str) -> String {
    format!(
        "note: [pskreporter] is off for file replay: {host} is not a loopback address, and a \
         recording's spots must not reach PSKReporter's public map"
    )
}

/// D10: an enabled `[pskreporter]`, unless this is a file replay to a host
/// that is not loopback; then it prints [`replay_note`] and returns `None`.
pub(crate) fn pskreporter_for_run(
    cfg: Option<&PskReporterConfig>,
    replay: bool,
) -> Option<PskReporterConfig> {
    let cfg = cfg?;
    if replay && !is_loopback_host(&cfg.host) {
        crate::logging::note(&replay_note(&cfg.host));
        return None;
    }
    Some(cfg.clone())
}

/// D9: the local address the PSKReporter socket binds: loopback for a
/// loopback host, the IPv4 wildcard otherwise (no DNS at startup).
pub(crate) fn pskreporter_bind_ip(host: &str) -> IpAddr {
    let peer = if is_loopback_host(host) {
        Ipv4Addr::LOCALHOST
    } else {
        Ipv4Addr::UNSPECIFIED
    };
    manta_input::local_bind::udp_local_addr(IpAddr::V4(peer))
}

/// D9: the PSKReporter socket, bound once for the session so its source
/// port stays fixed, as PSKReporter's page requires. Nonblocking.
pub(crate) fn bind_pskreporter_socket(host: &str) -> Result<UdpSocket> {
    let socket = UdpSocket::bind((pskreporter_bind_ip(host), 0))
        .and_then(|s| s.set_nonblocking(true).map(|()| s))
        .map_err(|e| anyhow!("[pskreporter] cannot open a UDP socket: {e}"))?;
    Ok(socket)
}

/// D13: one `[[wsjtx_output]]` socket: loopback for a loopback
/// destination, the wildcard otherwise; `SO_BROADCAST` on every IPv4
/// socket so a LAN broadcast address works. Nonblocking.
pub(crate) fn bind_wsjtx_output_socket(destination: SocketAddr) -> Result<UdpSocket> {
    let open = || -> std::io::Result<UdpSocket> {
        let socket = manta_input::local_bind::bind_udp_for(destination.ip())?;
        if destination.is_ipv4() {
            socket.set_broadcast(true)?;
        }
        socket.set_nonblocking(true)?;
        Ok(socket)
    };
    open().map_err(|e| {
        anyhow!("[[wsjtx_output]] cannot open a UDP socket to send to {destination}: {e}")
    })
}

/// The startup line of a running `[pskreporter]`.
pub(crate) fn pskreporter_startup_line(cfg: &PskReporterConfig, call: &str, grid: &str) -> String {
    format!(
        "pskreporter: reporting {} spots to {}:{} as {call} at {grid}; reports are public; the \
         first datagram goes out within 5.5 minutes, then at most one every 5 minutes",
        cfg.spot_types.as_str(),
        cfg.host,
        cfg.port,
    )
}

/// The startup line of a running `[[wsjtx_output]]`.
pub(crate) fn wsjtx_output_startup_line(destination: SocketAddr) -> String {
    format!(
        "wsjtx_output {destination}: sending spots as WSJT-X Status and Decode messages for LAN \
         loggers (one instance per band, \"manta-<band>\", heartbeat every 15 s)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn psk(host: &str) -> PskReporterConfig {
        PskReporterConfig {
            enabled: true,
            host: host.to_string(),
            ..PskReporterConfig::default()
        }
    }

    #[test]
    fn replay_allows_only_a_loopback_pskreporter_host() {
        for host in ["127.0.0.1", "127.1.2.3", "LOCALHOST"] {
            assert!(
                pskreporter_for_run(Some(&psk(host)), true).is_some(),
                "{host}"
            );
        }
        for host in ["report.pskreporter.info", "192.0.2.1", "10.0.0.1"] {
            assert!(
                pskreporter_for_run(Some(&psk(host)), true).is_none(),
                "{host}"
            );
            // A live source reports to any host.
            assert!(
                pskreporter_for_run(Some(&psk(host)), false).is_some(),
                "{host}"
            );
        }
        assert!(pskreporter_for_run(None, true).is_none());
    }

    #[test]
    fn a_disabled_table_still_registers_its_metrics_but_does_not_run() {
        let off = PskReporterConfig::default();
        let outputs = SpotOutputs::for_run(Some(&off), &[], false);
        assert!(outputs.pskreporter.is_none());
        assert!(outputs.pskreporter_registered);
        let replayed = SpotOutputs::for_run(Some(&psk("192.0.2.1")), &[], true);
        assert!(replayed.pskreporter.is_none());
        assert!(replayed.pskreporter_registered);
        assert!(!SpotOutputs::for_run(None, &[], false).pskreporter_registered);
    }

    #[test]
    fn a_loopback_host_binds_loopback_and_any_other_binds_the_wildcard() {
        let socket = bind_pskreporter_socket("127.0.0.1").unwrap();
        assert_eq!(
            socket.local_addr().unwrap().ip(),
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
        assert_eq!(
            pskreporter_bind_ip("localhost"),
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
        // MAN-467: the wildcard choice is asserted without binding it.
        assert_eq!(
            pskreporter_bind_ip("report.pskreporter.info"),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        );
        assert_eq!(
            pskreporter_bind_ip("192.0.2.1"),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        );
    }

    #[test]
    fn the_startup_line_names_host_call_and_grid() {
        assert_eq!(
            pskreporter_startup_line(&psk("report.pskreporter.info"), "W5AU", "EM10dh"),
            "pskreporter: reporting cq_beacon spots to report.pskreporter.info:4739 as W5AU at \
             EM10dh; reports are public; the first datagram goes out within 5.5 minutes, then at \
             most one every 5 minutes"
        );
        assert_eq!(
            wsjtx_output_startup_line("192.168.1.255:2237".parse().unwrap()),
            "wsjtx_output 192.168.1.255:2237: sending spots as WSJT-X Status and Decode messages \
             for LAN loggers (one instance per band, \"manta-<band>\", heartbeat every 15 s)"
        );
    }

    #[test]
    fn the_wsjtx_output_socket_binds_loopback_for_a_loopback_destination_and_sets_broadcast() {
        let socket = bind_wsjtx_output_socket("127.0.0.1:2237".parse().unwrap()).unwrap();
        assert_eq!(
            socket.local_addr().unwrap().ip(),
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
        assert!(socket.broadcast().unwrap());
    }
}
