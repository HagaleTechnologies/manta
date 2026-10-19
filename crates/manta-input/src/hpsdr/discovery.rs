//! Bounded Metis discovery, shared by LAN enumeration and source selection.
//!
//! P1 offsets follow piHPSDR old_discovery.c and hpsdr-rs discovery.rs;
//! P2 offsets follow piHPSDR new_discovery.c and sdroxide's discovery module.
//! The reply's source address, not its payload, supplies the radio's IP.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
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
pub fn broadcast_destinations(port: u16) -> [SocketAddr; 2] {
    [
        SocketAddr::from(([255, 255, 255, 255], port)),
        SocketAddr::from(([127, 255, 255, 255], port)),
    ]
}

fn remember(found: &mut BTreeMap<[u8; 6], DiscoveredRadio>, radio: DiscoveredRadio) {
    let entry = found.entry(radio.mac).or_insert_with(|| radio.clone());
    if radio.protocol > entry.protocol {
        *entry = radio;
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
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_broadcast(true)?;
    let start = Instant::now();
    let deadline = start + timeout;
    let mut rounds = 0;
    let mut found = BTreeMap::new();
    let mut buf = [0; 2048];
    while Instant::now() < deadline {
        if rounds < REQUEST_ROUNDS && start.elapsed() >= DISCOVERY_RETRY_INTERVAL * rounds {
            // Some platforms cannot route loopback broadcast. A successful
            // destination still yields a useful scan; report when all fail.
            let mut sent = false;
            let mut last_error = None;
            for dest in destinations {
                for request in [&discovery_request_p1()[..], &discovery_request_p2()[..]] {
                    match socket.send_to(request, dest) {
                        Ok(_) => sent = true,
                        Err(err) => last_error = Some(err),
                    }
                }
            }
            if !sent {
                return Err(last_error.unwrap_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "no discovery destinations")
                }));
            }
            rounds += 1;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        socket.set_read_timeout(Some(remaining.min(READ_SLICE)))?;
        match socket.recv_from(&mut buf) {
            Ok((n, addr)) if addr.port() == port => {
                if let Some(mut radio) = parse_reply(&buf[..n]) {
                    radio.ip = addr.ip();
                    remember(&mut found, radio);
                }
            }
            Ok(_) => {}
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(err) => return Err(err),
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
    /// The selected radio needs a protocol this driver cannot stream.
    Unsupported(DiscoveredRadio),
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
            Self::Unsupported(r) => write!(f, "the openHPSDR radio at {} (MAC {}, {}) speaks openHPSDR protocol 2, which this manta does not support yet; set input.host / --hpsdr-host to force a protocol-1 connection (MAN-305 adds protocol 2)", r.ip, format_mac(&r.mac), board_name(r.protocol, r.board_id)),
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
    if radio.protocol == DiscoveryProtocol::P2 {
        return Err(DiscoveryError::Unsupported(radio));
    }
    if radio.status == DiscoveryStatus::InUse {
        return Err(DiscoveryError::InUse(radio));
    }
    Ok(radio)
}

/// Discover once per open attempt, then select a lone available P1 radio.
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

#[cfg(test)]
mod tests {
    use super::*;
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
    fn select_reports_unsupported_for_a_p2_only_radio() {
        let err = select_found(vec![parse_reply(&p2()).unwrap()], None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("protocol 2"));
        assert!(err.contains("--hpsdr-host"));
    }

    #[test]
    fn select_reports_no_answer_on_an_empty_lan() {
        assert_eq!(select_found(vec![], None).unwrap_err().to_string(), "no openHPSDR radio answered discovery; run `manta devices`, or set input.host / --hpsdr-host");
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
}
