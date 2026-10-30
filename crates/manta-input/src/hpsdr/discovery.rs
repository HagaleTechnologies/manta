//! openHPSDR discovery: a bounded Metis broadcast that enumerates the LAN
//! and selects a radio when no address is configured (MAN-306), and a
//! directed probe of one configured radio that never broadcasts (MAN-305
//! D3, MAN-239).
//!
//! P1 offsets follow piHPSDR old_discovery.c and hpsdr-rs discovery.rs;
//! P2 offsets follow piHPSDR new_discovery.c and sdroxide's discovery
//! module. A broadcast reply's source address, not its payload, supplies
//! the radio's IP.

use anyhow::{anyhow, Context, Result};
use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

/// Total collection window, including retransmissions.
pub const DISCOVERY_TIMEOUT: Duration = Duration::from_millis(1500);
/// Spacing between the four request rounds.
pub const DISCOVERY_RETRY_INTERVAL: Duration = Duration::from_millis(250);
const REQUEST_ROUNDS: u32 = 4;
const READ_SLICE: Duration = Duration::from_millis(50);

/// Wire protocol used by a discovery response.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiscoveryProtocol {
    /// Original Metis protocol, supported by this driver.
    P1,
    /// openHPSDR protocol 2, listed but not streamed by this driver.
    P2,
}

/// Availability reported by the radio itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscoveryStatus {
    /// Ready for a client.
    Available,
    /// Already streaming to a client.
    InUse,
}

/// One radio, identified across interfaces and protocols by its MAC address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredRadio {
    /// Source address of the response (unspecified until received).
    pub ip: IpAddr,
    /// Hardware address used for deduplication and selection.
    pub mac: [u8; 6],
    /// Classified response format.
    pub protocol: DiscoveryProtocol,
    /// Board identifier in that protocol's table.
    pub board_id: u8,
    /// Firmware version byte, without guessing a board-specific version scheme.
    pub firmware: u8,
    /// Reported availability.
    pub status: DiscoveryStatus,
    /// P2 receiver count, absent in P1.
    pub receivers: Option<u8>,
}

/// Standard 63-byte Metis discovery request.
pub fn discovery_request_p1() -> [u8; 63] {
    let mut b = [0; 63];
    b[..3].copy_from_slice(&[0xef, 0xfe, 2]);
    b
}

/// Standard 60-byte protocol-2 discovery request.
pub fn discovery_request_p2() -> [u8; 60] {
    let mut b = [0; 60];
    b[4] = 2;
    b
}

/// Classify a length- and status-checked reply. The caller fills `ip`.
pub fn parse_reply(buf: &[u8]) -> Option<DiscoveredRadio> {
    let (protocol, status, mac, board_id, firmware, receivers) =
        if buf.len() >= 11 && buf[..2] == [0xef, 0xfe] {
            (
                DiscoveryProtocol::P1,
                buf[2],
                &buf[3..9],
                buf[10],
                buf[9],
                None,
            )
        } else if buf.len() >= 21 && buf[..4] == [0; 4] {
            (
                DiscoveryProtocol::P2,
                buf[4],
                &buf[5..11],
                buf[11],
                buf[13],
                Some(buf[20]),
            )
        } else {
            return None;
        };
    let status = match status {
        2 => DiscoveryStatus::Available,
        3 => DiscoveryStatus::InUse,
        _ => return None,
    };
    Some(DiscoveredRadio {
        ip: Ipv4Addr::UNSPECIFIED.into(),
        mac: mac.try_into().ok()?,
        protocol,
        board_id,
        firmware,
        status,
        receivers,
    })
}

/// Board names use separate tables: for example, ID 4 has different meanings.
pub fn board_name(protocol: DiscoveryProtocol, board_id: u8) -> &'static str {
    match (protocol, board_id) {
        (DiscoveryProtocol::P1, 0) => "Metis",
        (_, 1) => "Hermes",
        (DiscoveryProtocol::P1, 2) => "Griffin",
        (DiscoveryProtocol::P1, 4) | (DiscoveryProtocol::P2, 3) => "Angelia",
        (DiscoveryProtocol::P1, 5) | (DiscoveryProtocol::P2, 4) => "Orion",
        (DiscoveryProtocol::P1, 6) => "Hermes-Lite",
        (DiscoveryProtocol::P1, 10) | (DiscoveryProtocol::P2, 5) => "Orion II",
        (DiscoveryProtocol::P1, 100) => "STEMlab/Red Pitaya",
        (DiscoveryProtocol::P2, 0) => "Atlas",
        (DiscoveryProtocol::P2, 2) => "Hermes II",
        (DiscoveryProtocol::P2, 6) => "Hermes-Lite 2",
        (DiscoveryProtocol::P2, 10) => "Saturn",
        _ => "Unknown",
    }
}

/// Accept twelve hexadecimal digits, optionally separated by colons or dashes.
pub fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let digits: Vec<_> = text.bytes().filter(|b| !matches!(b, b':' | b'-')).collect();
    if digits.len() != 12 || !digits.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let mut mac = [0; 6];
    for (out, pair) in mac.iter_mut().zip(digits.as_chunks::<2>().0) {
        *out = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(mac)
}

/// Canonical, copyable uppercase colon-separated hardware address.
pub fn format_mac(mac: &[u8; 6]) -> String {
    mac.iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Shared operator-facing row for enumeration and selection failures.
pub fn describe_radio(r: &DiscoveredRadio) -> String {
    format!(
        "{}  MAC {}  {}  firmware {}  {}",
        r.ip,
        format_mac(&r.mac),
        board_name(r.protocol, r.board_id),
        r.firmware,
        match r.status {
            DiscoveryStatus::Available => "available",
            DiscoveryStatus::InUse => "in use",
        }
    )
}

/// LAN limited broadcast plus loopback for local radio simulators.
/// Hosts on another interface/subnet can use an explicit host address.
/// Linux delivers the loopback-directed broadcast to a simulator bound on
/// any address; macOS delivers neither broadcast to a local socket, so
/// `127.0.0.1` itself is asked too (a simulator heard twice is deduplicated
/// by MAC).
pub fn broadcast_destinations(port: u16) -> [SocketAddr; 3] {
    [
        SocketAddr::from(([255, 255, 255, 255], port)),
        SocketAddr::from(([127, 255, 255, 255], port)),
        SocketAddr::from(([127, 0, 0, 1], port)),
    ]
}

fn remember(found: &mut BTreeMap<[u8; 6], DiscoveredRadio>, radio: DiscoveredRadio) {
    let entry = found.entry(radio.mac).or_insert_with(|| radio.clone());
    if radio.protocol > entry.protocol {
        *entry = radio;
    }
}

/// Keep replies to loopback probes off the socket used for LAN broadcasts.
struct DiscoverySockets {
    loopback: Option<UdpSocket>,
    broadcast: Option<UdpSocket>,
}

fn open_discovery_sockets(destinations: &[SocketAddr]) -> io::Result<DiscoverySockets> {
    let open = |ip| -> io::Result<UdpSocket> {
        let socket = UdpSocket::bind((ip, 0))?;
        // Loopback broadcast (127.255.255.255) needs this too.
        socket.set_broadcast(true)?;
        Ok(socket)
    };
    Ok(DiscoverySockets {
        loopback: destinations
            .iter()
            .any(|dest| dest.ip().is_loopback())
            .then(|| open(Ipv4Addr::LOCALHOST))
            .transpose()?,
        broadcast: destinations
            .iter()
            .any(|dest| !dest.ip().is_loopback())
            .then(|| open(Ipv4Addr::UNSPECIFIED))
            .transpose()?,
    })
}

fn send_discovery_round(sockets: &DiscoverySockets, destinations: &[SocketAddr]) -> io::Result<()> {
    // Some platforms cannot route loopback broadcast. A successful
    // destination still yields a useful scan; report when all fail.
    let mut sent = false;
    let mut last_error = None;
    for (socket, loopback) in sockets
        .loopback
        .iter()
        .map(|s| (s, true))
        .chain(sockets.broadcast.iter().map(|s| (s, false)))
    {
        for dest in destinations
            .iter()
            .filter(|dest| dest.ip().is_loopback() == loopback)
        {
            for request in [&discovery_request_p1()[..], &discovery_request_p2()[..]] {
                match socket.send_to(request, dest) {
                    Ok(_) => sent = true,
                    Err(err) => last_error = Some(err),
                }
            }
        }
    }
    if sent {
        Ok(())
    } else {
        Err(last_error.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "no discovery destinations")
        }))
    }
}

/// Send both request shapes, then collect until the deadline. Destination
/// ports are supplied by the caller; `port` restricts replies to that radio
/// service. An I/O failure is distinct from a successful scan with no answers.
pub fn discover(
    port: u16,
    destinations: &[SocketAddr],
    timeout: Duration,
) -> io::Result<Vec<DiscoveredRadio>> {
    let sockets = open_discovery_sockets(destinations)?;
    if destinations.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no discovery destinations",
        ));
    }
    let socket_count =
        u32::from(sockets.loopback.is_some()) + u32::from(sockets.broadcast.is_some());
    let read_slice = READ_SLICE / socket_count;
    let start = Instant::now();
    let deadline = start + timeout;
    let mut rounds = 0;
    let mut found = BTreeMap::new();
    let mut buf = [0; 2048];
    while Instant::now() < deadline {
        if rounds < REQUEST_ROUNDS && start.elapsed() >= DISCOVERY_RETRY_INTERVAL * rounds {
            send_discovery_round(&sockets, destinations)?;
            rounds += 1;
        }
        // Divide the read slice across sockets so a mixed scan keeps the
        // same retry cadence and deadline as a single-socket scan.
        for socket in sockets.loopback.iter().chain(sockets.broadcast.iter()) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            socket.set_read_timeout(Some(remaining.min(read_slice)))?;
            match socket.recv_from(&mut buf) {
                Ok((n, addr)) if addr.port() == port => {
                    if let Some(mut radio) = parse_reply(&buf[..n]) {
                        radio.ip = addr.ip();
                        remember(&mut found, radio);
                    }
                }
                Ok(_) => {}
                // Includes Windows' ConnectionReset for an ICMP port-unreachable
                // when no simulator listens on 127.0.0.1.
                Err(err) if is_retry(err.kind()) => {}
                Err(err) => return Err(err),
            }
        }
    }
    Ok(found.into_values().collect())
}

/// Discovery failures, with actionable operator-facing diagnostics.
#[derive(Debug)]
pub enum DiscoveryError {
    /// The socket or network failed.
    Io(io::Error),
    /// No valid reply arrived.
    NoAnswer,
    /// The requested MAC was not among the responses.
    NoMatch {
        /// Requested hardware address.
        want: [u8; 6],
        /// Radios that did answer.
        found: Vec<DiscoveredRadio>,
    },
    /// The selected radio is already streaming.
    InUse(DiscoveredRadio),
    /// Several radios require an explicit choice.
    Ambiguous(Vec<DiscoveredRadio>),
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "openHPSDR discovery failed: {err}"),
            Self::NoAnswer => write!(f, "no openHPSDR radio answered discovery; run `manta devices`, or set input.host / --hpsdr-host"),
            Self::NoMatch { want, found } => {
                write!(f, "no openHPSDR radio with MAC {} answered discovery; check --hpsdr-mac / input.mac; radios found:", format_mac(want))?;
                for r in found { write!(f, "\n  {}", describe_radio(r))?; }
                Ok(())
            }
            Self::Ambiguous(found) => {
                write!(f, "multiple openHPSDR radios answered discovery ({}); choose one with --hpsdr-mac or input.mac:", found.len())?;
                for r in found { write!(f, "\n  {}", describe_radio(r))?; }
                Ok(())
            }
            Self::InUse(r) => write!(f, "the openHPSDR radio is in use: {}; stop the other client, or set input.host / --hpsdr-host to override", describe_radio(r)),
        }
    }
}

impl std::error::Error for DiscoveryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

fn select_found(
    found: Vec<DiscoveredRadio>,
    want_mac: Option<[u8; 6]>,
) -> Result<DiscoveredRadio, DiscoveryError> {
    if found.is_empty() {
        return Err(DiscoveryError::NoAnswer);
    }
    let candidates = match want_mac {
        Some(want) => {
            let matches: Vec<_> = found.iter().filter(|r| r.mac == want).cloned().collect();
            if matches.is_empty() {
                return Err(DiscoveryError::NoMatch { want, found });
            }
            matches
        }
        None => found,
    };
    if candidates.len() != 1 {
        return Err(DiscoveryError::Ambiguous(candidates));
    }
    let radio = candidates.into_iter().next().expect("one candidate");
    if radio.status == DiscoveryStatus::InUse {
        return Err(DiscoveryError::InUse(radio));
    }
    Ok(radio)
}

/// Discover once per open attempt, then select a lone available radio of
/// either protocol. The caller opens the selected address through
/// [`super::open_receiver`], whose `protocol` setting picks the driver (under
/// `Auto` its probe routes a protocol-2 radio to the MAN-305 driver).
pub fn select_radio(
    port: u16,
    destinations: &[SocketAddr],
    want_mac: Option<[u8; 6]>,
) -> Result<DiscoveredRadio, DiscoveryError> {
    select_found(
        discover(port, destinations, DISCOVERY_TIMEOUT).map_err(DiscoveryError::Io)?,
        want_mac,
    )
}

/// Length of the protocol-2 discovery request (D3).
pub const PROTOCOL2_REQUEST_LEN: usize = 60;
/// Length of the protocol-1 discovery request (D3).
pub const PROTOCOL1_REQUEST_LEN: usize = 63;
/// Discovery status: the radio is idle and can be started.
pub const STATUS_IDLE: u8 = 0x02;
/// Discovery status: the radio is already streaming to another program.
pub const STATUS_BUSY: u8 = 0x03;

/// Protocol-2 request `[4]`: the discovery command (D3).
const P2_REQUEST_COMMAND: usize = 4;
/// The protocol-2 discovery command value (D3).
const P2_DISCOVER: u8 = 0x02;
/// Protocol-1 request and reply signature `EF FE` at bytes 0..2 (D3).
const P1_SIGNATURE: [u8; 2] = [0xEF, 0xFE];
/// Protocol-1 request `[2]`: the discovery command (D3).
const P1_REQUEST_COMMAND: usize = 2;
/// The protocol-1 discovery command value (D3).
const P1_DISCOVER: u8 = 0x02;

/// Protocol-1 reply `[2]`: status (D3).
const P1_STATUS: usize = 2;
/// Protocol-1 reply `[3..9]`: MAC (D3).
const P1_MAC: usize = 3;
/// Protocol-1 reply `[9]`: firmware (D3).
const P1_FIRMWARE: usize = 9;
/// Protocol-1 reply `[10]`: board (D3).
const P1_BOARD: usize = 10;
/// Shortest protocol-1 reply carrying every field manta reads (D3).
const P1_MIN_REPLY_LEN: usize = P1_BOARD + 1;

/// Protocol-2 reply `[4]`: status (D3).
const P2_STATUS: usize = 4;
/// Protocol-2 reply `[5..11]`: MAC (D3).
const P2_MAC: usize = 5;
/// Protocol-2 reply `[11]`: board (D3, D7).
const P2_BOARD: usize = 11;
/// Protocol-2 reply `[12]`: protocol version (D3).
const P2_PROTOCOL_VERSION: usize = 12;
/// Protocol-2 reply `[13]`: firmware (D3).
const P2_FIRMWARE: usize = 13;
/// Protocol-2 reply `[20]`: DDC count (D3).
const P2_DDC_COUNT: usize = 20;
/// Shortest protocol-2 reply carrying every field manta reads (D3).
const P2_MIN_REPLY_LEN: usize = P2_DDC_COUNT + 1;

/// How long one `recv_from` waits before the probe loop rechecks its
/// clock: short against every D3 deadline.
const POLL: Duration = Duration::from_millis(10);
/// Receive buffer for one reply; larger than any discovery reply, so a
/// reply is never silently truncated.
const REPLY_BUF_LEN: usize = 2048;

/// One radio's answer to discovery (D3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryReply {
    /// `EF FE` reply: status byte 2, MAC 3..9, firmware 9, board 10.
    Protocol1 {
        status: u8,
        mac: [u8; 6],
        firmware: u8,
        board: u8,
    },
    /// Protocol-2 reply: status byte 4, MAC 5..11, board 11, protocol
    /// version 12, firmware 13, DDC count 20.
    Protocol2 {
        status: u8,
        mac: [u8; 6],
        board: u8,
        protocol_version: u8,
        firmware: u8,
        ddc_count: u8,
    },
}

/// What discovery found before a receiver was opened (D2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Discovery {
    Answered(DiscoveryReply),
    /// No reply within the window: protocol 1 is used.
    NoAnswer,
    /// A protocol was configured, so no discovery datagram was sent.
    Skipped,
}

/// Probe timing (D3); `Default` is the production 4 tries 250 ms apart in
/// a 1 s window, with a 100 ms grace for a protocol-2 reply after a
/// protocol-1 one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeTiming {
    pub tries: u32,
    pub interval: Duration,
    pub window: Duration,
    pub grace: Duration,
}

impl Default for ProbeTiming {
    fn default() -> Self {
        Self {
            tries: 4,
            interval: Duration::from_millis(250),
            window: Duration::from_secs(1),
            grace: Duration::from_millis(100),
        }
    }
}

/// The protocol-2 request: 60 bytes, all zero except byte 4 = `0x02`.
pub fn protocol2_request() -> [u8; PROTOCOL2_REQUEST_LEN] {
    let mut p = [0u8; PROTOCOL2_REQUEST_LEN];
    p[P2_REQUEST_COMMAND] = P2_DISCOVER;
    p
}

/// The protocol-1 request: `EF FE 02` then 60 zero bytes.
pub fn protocol1_request() -> [u8; PROTOCOL1_REQUEST_LEN] {
    let mut p = [0u8; PROTOCOL1_REQUEST_LEN];
    p[..P1_SIGNATURE.len()].copy_from_slice(&P1_SIGNATURE);
    p[P1_REQUEST_COMMAND] = P1_DISCOVER;
    p
}

/// Classify one reply datagram (D3); `None` for anything else.
pub fn classify_reply(buf: &[u8]) -> Option<DiscoveryReply> {
    let is_status = |b: u8| b == STATUS_IDLE || b == STATUS_BUSY;
    if buf.len() > P1_STATUS
        && buf[..P1_SIGNATURE.len()] == P1_SIGNATURE
        && is_status(buf[P1_STATUS])
    {
        if buf.len() < P1_MIN_REPLY_LEN {
            return None;
        }
        return Some(DiscoveryReply::Protocol1 {
            status: buf[P1_STATUS],
            mac: mac_at(buf, P1_MAC),
            firmware: buf[P1_FIRMWARE],
            board: buf[P1_BOARD],
        });
    }
    if buf.len() >= P2_MIN_REPLY_LEN && is_status(buf[P2_STATUS]) {
        return Some(DiscoveryReply::Protocol2 {
            status: buf[P2_STATUS],
            mac: mac_at(buf, P2_MAC),
            board: buf[P2_BOARD],
            protocol_version: buf[P2_PROTOCOL_VERSION],
            firmware: buf[P2_FIRMWARE],
            ddc_count: buf[P2_DDC_COUNT],
        });
    }
    None
}

/// The six MAC bytes at `buf[at..at + 6]`; the caller checked the length.
fn mac_at(buf: &[u8], at: usize) -> [u8; 6] {
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&buf[at..at + 6]);
    mac
}

/// The first IPv4 address `host` resolves to.
fn resolve_ipv4(host: &str, port: u16) -> Result<Ipv4Addr> {
    (host, port)
        .to_socket_addrs()
        .with_context(|| format!("resolving HPSDR radio {host}:{port} for discovery"))?
        .find_map(|a| match a {
            SocketAddr::V4(v4) => Some(*v4.ip()),
            SocketAddr::V6(_) => None,
        })
        .ok_or_else(|| anyhow!("HPSDR radio {host}:{port} has no IPv4 address for discovery"))
}

/// Probe `host:port` from a fresh socket, closed before this returns.
/// Returns `Answered` or `NoAnswer`, never `Skipped`.
pub fn probe(host: &str, port: u16, timing: &ProbeTiming) -> Result<Discovery> {
    let radio_ip = IpAddr::V4(resolve_ipv4(host, port)?);
    let radio = SocketAddr::new(radio_ip, port);
    let socket = crate::local_bind::bind_udp_for(radio_ip)
        .with_context(|| format!("binding a UDP socket for HPSDR discovery of {host}:{port}"))?;
    socket
        .set_read_timeout(Some(POLL))
        .with_context(|| format!("setting the HPSDR discovery read timeout for {host}:{port}"))?;
    let requests = [&protocol2_request()[..], &protocol1_request()[..]];
    let mut buf = [0u8; REPLY_BUF_LEN];
    let start = Instant::now();
    let mut sent = 0u32;
    // A protocol-1 reply and the end of its grace window.
    let mut fallback: Option<(DiscoveryReply, Instant)> = None;
    loop {
        let now = Instant::now();
        if let Some((reply, grace_end)) = fallback {
            // The grace runs to its end even past the window: the reply
            // itself arrived inside it.
            if now >= grace_end {
                return Ok(Discovery::Answered(reply));
            }
        } else if now.duration_since(start) >= timing.window {
            return Ok(Discovery::NoAnswer);
        }
        // Send the pair at t = 0, interval, 2 * interval, ... (`tries` times).
        if sent < timing.tries && now.duration_since(start) >= timing.interval * sent {
            for request in requests {
                match socket.send_to(request, radio) {
                    Ok(_) => {}
                    // Windows reports an earlier ICMP port-unreachable here.
                    Err(e) if e.kind() == ErrorKind::ConnectionReset => {}
                    Err(e) => {
                        return Err(e)
                            .with_context(|| format!("sending HPSDR discovery to {host}:{port}"))
                    }
                }
            }
            sent += 1;
        }
        let (n, src) = match socket.recv_from(&mut buf) {
            Ok(got) => got,
            Err(e) if is_retry(e.kind()) => continue,
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("receiving HPSDR discovery replies from {host}:{port}")
                })
            }
        };
        if src != radio {
            continue;
        }
        match classify_reply(&buf[..n]) {
            Some(reply @ DiscoveryReply::Protocol2 { .. }) => {
                return Ok(Discovery::Answered(reply))
            }
            // The first protocol-1 reply starts the grace; later ones do
            // not extend it. It also resends the protocol-2 request once:
            // the grace ends before the next scheduled try, so a lost
            // protocol-2 request or reply would otherwise settle a
            // dual-protocol radio on protocol 1 (D3, MAN-305 review).
            Some(reply @ DiscoveryReply::Protocol1 { .. }) if fallback.is_none() => {
                fallback = Some((reply, Instant::now() + timing.grace));
                match socket.send_to(&protocol2_request(), radio) {
                    Ok(_) => {}
                    Err(e) if e.kind() == ErrorKind::ConnectionReset => {}
                    Err(e) => {
                        return Err(e)
                            .with_context(|| format!("sending HPSDR discovery to {host}:{port}"))
                    }
                }
            }
            _ => {}
        }
    }
}

/// `recv_from` errors that mean "nothing yet, keep waiting": the read
/// timeout (`WouldBlock` on Unix, `TimedOut` on Windows), a signal, and
/// Windows' report of an earlier ICMP port-unreachable on an unconnected
/// UDP socket (`ConnectionReset`, D5).
fn is_retry(kind: ErrorKind) -> bool {
    matches!(
        kind,
        ErrorKind::WouldBlock
            | ErrorKind::TimedOut
            | ErrorKind::Interrupted
            | ErrorKind::ConnectionReset
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    fn p1(status: u8) -> Vec<u8> {
        let mut b = vec![0; 60];
        b[..3].copy_from_slice(&[0xef, 0xfe, status]);
        b[3..9].copy_from_slice(&[2, 17, 34, 51, 68, 85]);
        b[9] = 43;
        b[10] = 6;
        b[0x13..=0x15].copy_from_slice(&[4, 2, 1]);
        b
    }

    fn p2() -> Vec<u8> {
        let mut b = vec![0; 60];
        b[4] = 2;
        b[5..11].copy_from_slice(&[2, 17, 34, 51, 68, 85]);
        b[11] = 4;
        b[12] = 2;
        b[13] = 19;
        b[20] = 4;
        b
    }

    fn radio() -> DiscoveredRadio {
        let mut r = parse_reply(&p1(2)).unwrap();
        r.ip = "127.0.0.1".parse().unwrap();
        r
    }

    #[test]
    fn p1_request_is_63_bytes_ef_fe_02_then_zeros() {
        let b = discovery_request_p1();
        assert_eq!(b.len(), 63);
        assert_eq!(&b[..3], &[0xef, 0xfe, 2]);
        assert!(b[3..].iter().all(|v| *v == 0));
    }

    #[test]
    fn p2_request_is_60_zeros_with_byte4_02() {
        let mut expected = [0; 60];
        expected[4] = 2;
        assert_eq!(discovery_request_p2(), expected);
    }

    #[test]
    fn parses_a_hermes_lite_2_p1_reply() {
        let r = parse_reply(&p1(3)).unwrap();
        assert_eq!(r.mac, [2, 17, 34, 51, 68, 85]);
        assert_eq!(r.protocol, DiscoveryProtocol::P1);
        assert_eq!(r.status, DiscoveryStatus::InUse);
        assert_eq!(r.board_id, 6);
        assert_eq!(r.firmware, 43);
        assert_eq!(r.receivers, None);
    }

    #[test]
    fn parses_a_p2_reply() {
        let r = parse_reply(&p2()).unwrap();
        assert_eq!(r.protocol, DiscoveryProtocol::P2);
        assert_eq!(r.mac, radio().mac);
        assert_eq!(r.board_id, 4);
        assert_eq!(r.firmware, 19);
        assert_eq!(r.receivers, Some(4));
    }

    #[test]
    fn ignores_short_and_unsynced_replies() {
        for b in [p1(2), p2()] {
            let min = if b[0] == 0xef { 11 } else { 21 };
            for n in 0..min {
                assert!(parse_reply(&b[..n]).is_none());
            }
            assert!(parse_reply(&b[..min]).is_some());
        }
        for status in [0, 1, 4, 255] {
            assert!(parse_reply(&p1(status)).is_none());
        }
        let mut b = p2();
        b[4] = 4;
        assert!(parse_reply(&b).is_none());
        b = p1(2);
        b[0] = 0;
        assert!(parse_reply(&b).is_none());
        assert!(parse_reply(&[0xff; 60]).is_none());
    }

    #[test]
    fn board_tables_are_per_protocol() {
        assert_eq!(board_name(DiscoveryProtocol::P1, 4), "Angelia");
        assert_eq!(board_name(DiscoveryProtocol::P2, 4), "Orion");
    }

    #[test]
    fn board_name_covers_the_documented_ids() {
        for (id, name) in [
            (0, "Metis"),
            (1, "Hermes"),
            (2, "Griffin"),
            (4, "Angelia"),
            (5, "Orion"),
            (6, "Hermes-Lite"),
            (10, "Orion II"),
            (100, "STEMlab/Red Pitaya"),
        ] {
            assert_eq!(board_name(DiscoveryProtocol::P1, id), name);
        }
        for (id, name) in [
            (0, "Atlas"),
            (1, "Hermes"),
            (2, "Hermes II"),
            (3, "Angelia"),
            (4, "Orion"),
            (5, "Orion II"),
            (6, "Hermes-Lite 2"),
            (10, "Saturn"),
        ] {
            assert_eq!(board_name(DiscoveryProtocol::P2, id), name);
        }
        assert_eq!(board_name(DiscoveryProtocol::P1, 255), "Unknown");
    }

    #[test]
    fn parses_colon_dash_and_bare_hex_macs_case_insensitively() {
        for text in [
            "aa:BB:cc:DD:ee:FF",
            "aa-bb-cc-dd-ee-ff",
            "aabbccddeeff",
            "aa-bbcc-ddee-ff",
        ] {
            assert_eq!(parse_mac(text), Some([170, 187, 204, 221, 238, 255]));
        }
        for text in [
            "",
            "AA",
            "aa:bb:cc:dd:ee:gg",
            "00112233445566",
            " 001122334455",
            "éééééé",
        ] {
            assert_eq!(parse_mac(text), None);
        }
    }

    #[test]
    fn formats_the_canonical_uppercase_colon_form() {
        assert_eq!(
            format_mac(&[10, 187, 12, 221, 14, 255]),
            "0A:BB:0C:DD:0E:FF"
        );
    }

    #[test]
    fn dedupes_by_mac_keeping_the_higher_protocol() {
        let mut found = BTreeMap::new();
        let a = radio();
        let mut b = parse_reply(&p2()).unwrap();
        b.ip = "192.0.2.1".parse().unwrap();
        remember(&mut found, a.clone());
        remember(&mut found, b.clone());
        remember(&mut found, a);
        assert_eq!(found.into_values().collect::<Vec<_>>(), vec![b]);
    }

    #[test]
    fn select_returns_the_lone_available_radio() {
        assert_eq!(select_found(vec![radio()], None).unwrap(), radio());
    }

    #[test]
    fn select_refuses_a_lone_in_use_radio() {
        let mut r = radio();
        r.status = DiscoveryStatus::InUse;
        let err = select_found(vec![r], None).unwrap_err().to_string();
        assert!(err.contains("in use"));
        assert!(err.contains("02:11:22:33:44:55"));
        assert!(err.contains("--hpsdr-host"));
    }

    #[test]
    fn select_lists_ambiguous_radios_with_the_exact_error_text() {
        let a = radio();
        let mut b = a.clone();
        b.mac = [2, 187, 187, 187, 187, 2];
        b.status = DiscoveryStatus::InUse;
        assert_eq!(select_found(vec![a, b], None).unwrap_err().to_string(),
            "multiple openHPSDR radios answered discovery (2); choose one with --hpsdr-mac or input.mac:\n  127.0.0.1  MAC 02:11:22:33:44:55  Hermes-Lite  firmware 43  available\n  127.0.0.1  MAC 02:BB:BB:BB:BB:02  Hermes-Lite  firmware 43  in use");
    }

    #[test]
    fn select_filters_by_mac_and_reports_a_no_match_listing() {
        let a = radio();
        let mut b = a.clone();
        b.mac[5] = 99;
        assert_eq!(select_found(vec![a.clone(), b], Some(a.mac)).unwrap(), a);
        let err = select_found(vec![a], Some([0; 6])).unwrap_err().to_string();
        assert!(err.contains("00:00:00:00:00:00"));
        assert!(err.contains("02:11:22:33:44:55"));
    }

    #[test]
    fn select_returns_a_lone_available_p2_radio_for_the_protocol2_driver() {
        let p2 = parse_reply(&p2()).unwrap();
        assert_eq!(p2.protocol, DiscoveryProtocol::P2);
        assert_eq!(select_found(vec![p2.clone()], None).unwrap(), p2);
        assert_eq!(select_found(vec![p2.clone()], Some(p2.mac)).unwrap(), p2);
    }

    #[test]
    fn select_reports_no_answer_on_an_empty_lan() {
        assert_eq!(select_found(vec![], None).unwrap_err().to_string(), "no openHPSDR radio answered discovery; run `manta devices`, or set input.host / --hpsdr-host");
    }

    #[test]
    fn loopback_only_destinations_open_no_wildcard_socket() {
        let destinations = [
            SocketAddr::from(([127, 0, 0, 1], 1024)),
            SocketAddr::from(([127, 255, 255, 255], 1024)),
        ];
        let sockets = open_discovery_sockets(&destinations).unwrap();
        assert!(sockets.broadcast.is_none());
        let loopback = sockets.loopback.unwrap();
        assert_eq!(
            loopback.local_addr().unwrap().ip(),
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
        assert!(loopback.broadcast().unwrap());
    }

    #[test]
    fn broadcast_destinations_keep_a_wildcard_broadcast_socket() {
        let sockets = open_discovery_sockets(&broadcast_destinations(1024)).unwrap();
        let broadcast = sockets.broadcast.unwrap();
        assert_eq!(
            broadcast.local_addr().unwrap().ip(),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        );
        assert!(broadcast.broadcast().unwrap());
        let loopback = sockets.loopback.unwrap();
        assert_eq!(
            loopback.local_addr().unwrap().ip(),
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
        // 127.255.255.255 is sent on this socket and needs SO_BROADCAST too.
        assert!(loopback.broadcast().unwrap());
    }

    #[test]
    fn broadcast_only_destinations_open_no_loopback_socket() {
        let destinations = [SocketAddr::from(([255, 255, 255, 255], 1024))];
        let sockets = open_discovery_sockets(&destinations).unwrap();
        assert!(sockets.loopback.is_none());
        assert_eq!(
            sockets.broadcast.unwrap().local_addr().unwrap().ip(),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        );
    }

    #[test]
    fn discover_finds_a_loopback_fake_with_broadcast_destinations_present() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let dest = socket.local_addr().unwrap();
        let responder = thread::spawn(move || {
            let mut buf = [0; 100];
            let (n, addr) = socket.recv_from(&mut buf).unwrap();
            assert_eq!(&buf[..n], &discovery_request_p1());
            socket.send_to(&p1(2), addr).unwrap();
        });
        let destinations = broadcast_destinations(dest.port());
        let found = discover(dest.port(), &destinations, Duration::from_millis(200)).unwrap();
        responder.join().unwrap();
        assert_eq!(found, vec![radio()]);
    }

    #[test]
    fn discover_finds_a_fake_radio_on_a_loopback_destination() {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let dest = sock.local_addr().unwrap();
        let h = thread::spawn(move || {
            let mut b = [0; 100];
            let (n, addr) = sock.recv_from(&mut b).unwrap();
            assert_eq!(&b[..n], &discovery_request_p1());
            sock.send_to(&p1(2), addr).unwrap();
        });
        let found = discover(dest.port(), &[dest], Duration::from_millis(100)).unwrap();
        h.join().unwrap();
        assert_eq!(found, vec![radio()]);
    }

    #[test]
    fn discover_is_bounded_when_nothing_answers() {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let dest = sock.local_addr().unwrap();
        let start = Instant::now();
        assert!(discover(dest.port(), &[dest], DISCOVERY_TIMEOUT)
            .unwrap()
            .is_empty());
        assert!(start.elapsed() < Duration::from_millis(2500));
    }

    #[test]
    fn discover_with_broadcast_is_bounded_when_nothing_answers() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket.local_addr().unwrap().port();
        let start = Instant::now();
        assert!(
            discover(port, &broadcast_destinations(port), DISCOVERY_TIMEOUT)
                .unwrap()
                .is_empty()
        );
        assert!(start.elapsed() < Duration::from_millis(2500));
    }

    #[test]
    fn discover_retries_both_protocols_four_times() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let dest = socket.local_addr().unwrap();
        let receiver = thread::spawn(move || {
            let mut counts = [0, 0];
            for _ in 0..8 {
                let mut buf = [0; 100];
                let (n, _) = socket.recv_from(&mut buf).unwrap();
                if buf[..n] == discovery_request_p1() {
                    counts[0] += 1;
                } else {
                    assert_eq!(&buf[..n], &discovery_request_p2());
                    counts[1] += 1;
                }
            }
            counts
        });
        assert!(discover(dest.port(), &[dest], DISCOVERY_TIMEOUT)
            .unwrap()
            .is_empty());
        assert_eq!(receiver.join().unwrap(), [4, 4]);
    }

    #[test]
    fn discovery_io_failure_is_distinct_from_an_empty_lan() {
        let error = discover(1024, &[], DISCOVERY_TIMEOUT).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(DiscoveryError::Io(error)
            .to_string()
            .contains("discovery failed"));
    }

    #[test]
    #[ignore = "needs a real openHPSDR radio on the LAN (MAN-52)"]
    fn discovers_a_real_radio() {
        assert!(!discover(
            super::super::CONTROL_PORT,
            &broadcast_destinations(super::super::CONTROL_PORT),
            DISCOVERY_TIMEOUT
        )
        .unwrap()
        .is_empty());
    }

    const MAC: [u8; 6] = [0x00, 0x1C, 0xC0, 0xA2, 0x13, 0x5D];

    fn p1_reply(status: u8) -> Vec<u8> {
        let mut r = vec![0u8; 60];
        r[0..3].copy_from_slice(&[0xEF, 0xFE, status]);
        r[3..9].copy_from_slice(&MAC);
        r[9] = 73; // firmware
        r[10] = 6; // board: Hermes Lite
        r
    }

    fn p2_reply(status: u8) -> Vec<u8> {
        let mut r = vec![0u8; 60];
        r[4] = status;
        r[5..11].copy_from_slice(&MAC);
        r[11] = 5; // board: Orion II
        r[12] = 39; // protocol version
        r[13] = 21; // firmware
        r[20] = 1; // DDC count
        r
    }

    fn is_p2_request(d: &[u8]) -> bool {
        d == protocol2_request()
    }

    fn is_p1_request(d: &[u8]) -> bool {
        d == protocol1_request()
    }

    fn fast() -> ProbeTiming {
        ProbeTiming {
            tries: 4,
            interval: Duration::from_millis(50),
            window: Duration::from_millis(300),
            grace: Duration::from_millis(30),
        }
    }

    /// Run `probe` against a fake radio on 127.0.0.1 at an ephemeral port.
    /// `respond` sees each datagram the radio receives (with its source)
    /// and the radio's socket. Returns the probe's result, how long it took
    /// and every datagram the radio received.
    fn probe_fake<F>(timing: ProbeTiming, mut respond: F) -> (Discovery, Duration, Vec<Vec<u8>>)
    where
        F: FnMut(&UdpSocket, &[u8], SocketAddr) + Send,
    {
        let radio = UdpSocket::bind("127.0.0.1:0").unwrap();
        radio
            .set_read_timeout(Some(Duration::from_millis(5)))
            .unwrap();
        let port = radio.local_addr().unwrap().port();
        let done = AtomicBool::new(false);
        thread::scope(|s| {
            let fake = s.spawn(|| {
                let mut seen = Vec::new();
                let mut buf = [0u8; 2048];
                while !done.load(Ordering::SeqCst) {
                    if let Ok((n, src)) = radio.recv_from(&mut buf) {
                        seen.push(buf[..n].to_vec());
                        respond(&radio, &buf[..n], src);
                    }
                }
                seen
            });
            let start = Instant::now();
            let result = probe("127.0.0.1", port, &timing);
            let took = start.elapsed();
            done.store(true, Ordering::SeqCst);
            let seen = fake.join().unwrap();
            (result.unwrap(), took, seen)
        })
    }

    #[test]
    fn discovery_requests_bytes() {
        let p2 = protocol2_request();
        assert_eq!(p2.len(), 60);
        assert_eq!(p2[4], 0x02);
        assert!(p2.iter().enumerate().all(|(i, &b)| i == 4 || b == 0));
        let p1 = protocol1_request();
        assert_eq!(p1.len(), 63);
        assert_eq!(&p1[0..3], &[0xEF, 0xFE, 0x02]);
        assert!(p1[3..].iter().all(|&b| b == 0));
    }

    #[test]
    fn classifies_protocol_1_reply() {
        for status in [STATUS_IDLE, STATUS_BUSY] {
            assert_eq!(
                classify_reply(&p1_reply(status)),
                Some(DiscoveryReply::Protocol1 {
                    status,
                    mac: MAC,
                    firmware: 73,
                    board: 6,
                })
            );
        }
        // Eleven bytes carry every field.
        assert!(matches!(
            classify_reply(&p1_reply(STATUS_IDLE)[..11]),
            Some(DiscoveryReply::Protocol1 { board: 6, .. })
        ));
    }

    #[test]
    fn classifies_protocol_2_idle_and_busy_replies() {
        for status in [STATUS_IDLE, STATUS_BUSY] {
            assert_eq!(
                classify_reply(&p2_reply(status)),
                Some(DiscoveryReply::Protocol2 {
                    status,
                    mac: MAC,
                    board: 5,
                    protocol_version: 39,
                    firmware: 21,
                    ddc_count: 1,
                })
            );
        }
        // Twenty-one bytes carry every field.
        assert!(matches!(
            classify_reply(&p2_reply(STATUS_BUSY)[..21]),
            Some(DiscoveryReply::Protocol2 {
                status: STATUS_BUSY,
                ddc_count: 1,
                ..
            })
        ));
    }

    #[test]
    fn ignores_short_or_unknown_replies() {
        assert_eq!(classify_reply(&[]), None);
        assert_eq!(classify_reply(&[0xEF, 0xFE]), None);
        assert_eq!(classify_reply(&p1_reply(STATUS_IDLE)[..10]), None);
        assert_eq!(classify_reply(&p2_reply(STATUS_IDLE)[..20]), None);
        for status in [0x00, 0x01, 0x04, 0xFF] {
            assert_eq!(classify_reply(&p2_reply(status)), None, "p2 {status}");
            let mut p1 = p1_reply(STATUS_IDLE);
            p1[2] = status;
            // Not protocol 1; byte 4 is 0, so not protocol 2 either.
            assert_eq!(classify_reply(&p1), None, "p1 {status}");
        }
        assert_eq!(classify_reply(&[0xAB; 60]), None);
    }

    #[test]
    fn auto_retries_discovery_four_times_within_the_window() {
        let (result, took, seen) = probe_fake(fast(), |_, _, _| {});
        assert_eq!(result, Discovery::NoAnswer);
        assert!(took >= fast().window, "{took:?}");
        assert_eq!(seen.iter().filter(|d| is_p2_request(d)).count(), 4);
        assert_eq!(seen.iter().filter(|d| is_p1_request(d)).count(), 4);
        assert_eq!(seen.len(), 8, "nothing but the two requests");
    }

    #[test]
    fn discovery_ignores_replies_from_other_addresses_or_ports() {
        // The right address, the wrong source port.
        let other_port = UdpSocket::bind("127.0.0.1:0").unwrap();
        let (result, _, seen) = probe_fake(fast(), |_, d, src| {
            if is_p2_request(d) {
                other_port.send_to(&p2_reply(STATUS_IDLE), src).unwrap();
            }
        });
        assert_eq!(result, Discovery::NoAnswer);
        assert!(!seen.is_empty());

        // The right source port, the wrong address: Linux routes all of
        // 127.0.0.0/8 to loopback, so 127.0.0.2 can send to 127.0.0.1.
        #[cfg(target_os = "linux")]
        {
            let mut other_addr: Option<UdpSocket> = None;
            let (result, _, _) = probe_fake(fast(), |radio, d, src| {
                if is_p2_request(d) {
                    let port = radio.local_addr().unwrap().port();
                    let s = other_addr
                        .get_or_insert_with(|| UdpSocket::bind(("127.0.0.2", port)).unwrap());
                    s.send_to(&p2_reply(STATUS_IDLE), src).unwrap();
                }
            });
            assert_eq!(result, Discovery::NoAnswer);
        }
    }

    #[test]
    fn probe_returns_protocol_2_reply() {
        let (result, took, _) = probe_fake(fast(), |radio, d, src| {
            if is_p2_request(d) {
                radio.send_to(&p2_reply(STATUS_IDLE), src).unwrap();
            }
        });
        assert_eq!(
            result,
            Discovery::Answered(DiscoveryReply::Protocol2 {
                status: STATUS_IDLE,
                mac: MAC,
                board: 5,
                protocol_version: 39,
                firmware: 21,
                ddc_count: 1,
            })
        );
        assert!(
            took < fast().window,
            "the first reply ends discovery: {took:?}"
        );
    }

    #[test]
    fn probe_prefers_protocol_2_within_the_grace_window() {
        let timing = ProbeTiming {
            grace: Duration::from_millis(100),
            ..fast()
        };
        let mut answered = false;
        let (result, _, _) = probe_fake(timing, |radio, _, src| {
            if !answered {
                answered = true;
                radio.send_to(&p1_reply(STATUS_IDLE), src).unwrap();
                thread::sleep(Duration::from_millis(10));
                radio.send_to(&p2_reply(STATUS_BUSY), src).unwrap();
            }
        });
        assert!(
            matches!(
                result,
                Discovery::Answered(DiscoveryReply::Protocol2 {
                    status: STATUS_BUSY,
                    board: 5,
                    ..
                })
            ),
            "{result:?}"
        );
    }

    #[test]
    fn a_protocol_1_reply_resends_the_protocol_2_request_inside_the_grace() {
        // A dual-protocol radio whose first protocol-2 exchange is lost:
        // the grace (30 ms) ends long before the next try (250 ms), so only
        // the resend triggered by the protocol-1 reply can still find it.
        let timing = ProbeTiming {
            tries: 4,
            interval: Duration::from_millis(250),
            window: Duration::from_secs(1),
            grace: Duration::from_millis(30),
        };
        let mut p2_requests = 0;
        let (result, took, seen) = probe_fake(timing, |radio, d, src| {
            if is_p1_request(d) {
                radio.send_to(&p1_reply(STATUS_IDLE), src).unwrap();
            } else if is_p2_request(d) {
                p2_requests += 1;
                if p2_requests > 1 {
                    radio.send_to(&p2_reply(STATUS_IDLE), src).unwrap();
                }
            }
        });
        assert!(
            matches!(
                result,
                Discovery::Answered(DiscoveryReply::Protocol2 { board: 5, .. })
            ),
            "{result:?}"
        );
        assert!(took < timing.interval, "{took:?}");
        assert_eq!(seen.iter().filter(|d| is_p2_request(d)).count(), 2);
    }

    #[test]
    fn probe_returns_protocol_1_after_the_grace_window() {
        let (result, took, _) = probe_fake(fast(), |radio, d, src| {
            if is_p1_request(d) {
                radio.send_to(&p1_reply(STATUS_IDLE), src).unwrap();
            }
        });
        assert_eq!(
            result,
            Discovery::Answered(DiscoveryReply::Protocol1 {
                status: STATUS_IDLE,
                mac: MAC,
                firmware: 73,
                board: 6,
            })
        );
        assert!(took >= fast().grace, "{took:?}");
        assert!(took < fast().window, "{took:?}");
    }

    #[test]
    fn probe_falls_back_to_no_answer() {
        // A radio that answers only with datagrams that are not replies.
        let (result, took, _) = probe_fake(fast(), |radio, _, src| {
            radio.send_to(&[0x55; 10], src).unwrap();
            radio.send_to(&p2_reply(0x00), src).unwrap();
        });
        assert_eq!(result, Discovery::NoAnswer);
        assert!(took >= fast().window, "{took:?}");
    }
}
