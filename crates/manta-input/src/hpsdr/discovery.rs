//! openHPSDR discovery of one configured radio (MAN-305 D3): a directed
//! probe sending both the protocol-1 and the protocol-2 request to
//! `host:port`, never a broadcast (MAN-239). Run by
//! [`super::open_receiver`] under [`super::HpsdrProtocol::Auto`] only.

use anyhow::{anyhow, Context, Result};
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

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
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
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
