//! ka9q-radio (radiod) multicast IQ input (MAN-140).
//!
//! radiod turns one SDR into many independently tuned channels and sends
//! each as RTP over UDP multicast. A channel in `mode = iq` is complex
//! baseband, the same kind of data every other manta input delivers.
//! [`Ka9qIqSource`] joins one existing channel's data group, learns the
//! channel's sample rate, encoding and frequency from the status packets
//! radiod sends to that same group on port 5006, and hands its IQ samples
//! to the engine. It never creates or retunes channels.
//!
//! Provenance (D1): ka9q-radio is GPL-3.0; this module is written from the
//! protocol facts recorded in docs/DECISIONS/2026-10-13-man140-ka9q-radio-input.md
//! (documented behaviour, enum values, wire types, port numbers). No
//! ka9q-radio code is copied or translated.

pub mod wire;

use crate::{InputHealthCounters, IqSource, MAX_IQ_WAV_RATE_HZ};
use anyhow::{bail, Context, Result};
use num_complex::Complex32;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::collections::{BTreeMap, VecDeque};
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddrV4, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use wire::{ChannelStatus, Encoding, SeqObservation, SeqTracker};

/// Per-`recv` timeout, so `read()` re-checks status and its stall deadline
/// at least four times a second. Same value as `HpsdrIqSource`'s and
/// `KiwiIqSource`'s `READ_TIMEOUT`; kept separate because HPSDR is
/// feature-gated and must not become a dependency of this module.
const READ_TIMEOUT: Duration = Duration::from_millis(250);

/// One reused receive buffer sized to the largest UDP payload, so `recv`
/// can never truncate a datagram (HPSDR's `RECV_BUF_LEN` reasoning).
const RECV_BUF_LEN: usize = 65536;

/// Requested kernel receive buffer; Linux grants `net.core.rmem_max`
/// (416 KiB by default) and a smaller grant is not an error (D9).
const RCVBUF_BYTES: usize = 4 << 20;

/// The window `SeqTracker` treats as loss rather than a restart (D11):
/// as many packets as fit in 10 s, the HPSDR/Kiwi stall bound.
const SEQ_WINDOW_S: f64 = 10.0;

/// The most channels `open` remembers while choosing one (D7), so status
/// packets with ever-new SSRCs from a host on the group cannot grow memory
/// for the whole open wait.
const MAX_SEEN_CHANNELS: usize = 64;

/// Where and how to join one radiod channel.
#[derive(Debug, Clone)]
pub struct Ka9qConfig {
    /// The channel's data group: an IPv4 multicast literal or a host name.
    pub group: String,
    /// RTP data port (radiod's default [`wire::DATA_PORT`]).
    pub port: u16,
    /// The channel to read when the group carries more than one.
    pub ssrc: Option<u32>,
    /// Local IPv4 address to join on; `None` is the kernel's choice.
    pub interface: Option<Ipv4Addr>,
    /// Status port; radiod always uses [`wire::STATUS_PORT`]. A field only
    /// so tests can use free ports (D20).
    pub status_port: u16,
    /// How long `open` waits for the channel's status (D8).
    pub open_timeout: Duration,
    /// With no `ssrc`, how long `open` keeps listening after the first
    /// status packet before choosing (D7).
    pub settle: Duration,
    /// How long `read` waits without an accepted packet before failing (D10).
    pub stall_timeout: Duration,
}

impl Ka9qConfig {
    pub fn new(group: impl Into<String>) -> Self {
        Self {
            group: group.into(),
            port: wire::DATA_PORT,
            ssrc: None,
            interface: None,
            status_port: wire::STATUS_PORT,
            open_timeout: Duration::from_secs(10),
            settle: Duration::from_millis(1500),
            stall_timeout: Duration::from_secs(10),
        }
    }
}

/// Checks a `group` string without resolving it: an IPv4 multicast
/// literal, an IPv6 literal (refused), or a syntactically valid host name.
/// The message has no key prefix; callers add `input.group` or the flag.
pub fn check_group(group: &str) -> std::result::Result<(), String> {
    if let Ok(a) = group.parse::<Ipv4Addr>() {
        return if a.is_multicast() {
            Ok(())
        } else {
            Err(not_multicast(group))
        };
    }
    if group.contains(':') {
        return Err(format!(
            "IPv6 groups are not supported yet; give radiod's IPv4 data group, got {group}"
        ));
    }
    let name = group.strip_suffix('.').unwrap_or(group);
    let label_ok = |l: &str| {
        (1..=63).contains(&l.len()) && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    };
    if name.is_empty() || name.len() > 253 || !name.split('.').all(label_ok) {
        return Err(not_multicast(group));
    }
    Ok(())
}

fn not_multicast(group: &str) -> String {
    format!(
        "must be an IPv4 multicast address (224.0.0.0 to 239.255.255.255) or a host name, got {group}"
    )
}

/// The group as an IPv4 multicast address: a literal directly, a name
/// through the system resolver.
fn resolve_group(group: &str, port: u16) -> Result<Ipv4Addr> {
    check_group(group).map_err(|e| anyhow::anyhow!("ka9q-radio group {e}"))?;
    if let Ok(a) = group.parse::<Ipv4Addr>() {
        return Ok(a);
    }
    let addrs: Vec<_> = (group, port)
        .to_socket_addrs()
        .map_err(|e| {
            anyhow::anyhow!(
                "could not resolve ka9q-radio group {group}: {e}; a radiod .local name needs mDNS on this machine (Avahi with nss-mdns on Linux), or give the address avahi-browse -rt _rtp._udp lists"
            )
        })?
        .collect();
    let v4: Vec<Ipv4Addr> = addrs
        .iter()
        .filter_map(|a| match a.ip() {
            IpAddr::V4(v) => Some(v),
            IpAddr::V6(_) => None,
        })
        .collect();
    if let Some(m) = v4.iter().find(|a| a.is_multicast()) {
        return Ok(*m);
    }
    match v4.first() {
        Some(a) => bail!(
            "ka9q-radio group {group} resolved to {a}, which is not an IPv4 multicast address"
        ),
        None => bail!(
            "could not resolve ka9q-radio group {group}: no IPv4 address; a radiod .local name needs mDNS on this machine (Avahi with nss-mdns on Linux), or give the address avahi-browse -rt _rtp._udp lists"
        ),
    }
}

/// The interfaces to join on (D9): the configured one or the kernel's
/// choice, plus loopback for a radiod on this machine at `ttl = 0`.
fn join_interfaces(interface: Option<Ipv4Addr>) -> Vec<Ipv4Addr> {
    let first = interface.unwrap_or(Ipv4Addr::UNSPECIFIED);
    if first == Ipv4Addr::LOCALHOST {
        vec![first]
    } else {
        vec![first, Ipv4Addr::LOCALHOST]
    }
}

/// D9's rule over the join results: a configured interface must join; with
/// none configured, one successful join is enough.
fn check_joins(
    interface: Option<Ipv4Addr>,
    results: &[(Ipv4Addr, std::result::Result<(), String>)],
) -> std::result::Result<(), String> {
    if let Some(want) = interface {
        if let Some((_, Err(e))) = results.iter().find(|(a, _)| *a == want) {
            return Err(format!("could not join on interface {want}: {e}"));
        }
    }
    if results.iter().all(|(_, r)| r.is_err()) {
        let all: Vec<String> = results
            .iter()
            .map(|(a, r)| format!("{a}: {}", r.as_ref().err().map_or("", |e| e.as_str())))
            .collect();
        return Err(format!(
            "could not join on any interface ({})",
            all.join(", ")
        ));
    }
    Ok(())
}

fn join_socket(group: Ipv4Addr, port: u16, interface: Option<Ipv4Addr>) -> Result<UdpSocket> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .context("ka9q-radio: create UDP socket")?;
    s.set_reuse_address(true)
        .context("ka9q-radio: SO_REUSEADDR")?;
    #[cfg(all(unix, not(any(target_os = "solaris", target_os = "illumos"))))]
    s.set_reuse_port(true).context("ka9q-radio: SO_REUSEPORT")?;
    let _ = s.set_recv_buffer_size(RCVBUF_BYTES);
    // Unix binds the group so the socket sees only that group; Windows
    // cannot bind a multicast address and relies on the SSRC filter.
    #[cfg(unix)]
    let bind = SocketAddrV4::new(group, port);
    #[cfg(not(unix))]
    let bind = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port);
    s.bind(&SockAddr::from(bind))
        .with_context(|| format!("ka9q-radio: bind {bind}"))?;
    let results: Vec<_> = join_interfaces(interface)
        .into_iter()
        .map(|i| {
            (
                i,
                s.join_multicast_v4(&group, &i).map_err(|e| e.to_string()),
            )
        })
        .collect();
    check_joins(interface, &results)
        .map_err(|e| anyhow::anyhow!("ka9q-radio group {group}:{port}: {e}"))?;
    let u: UdpSocket = s.into();
    u.set_read_timeout(Some(READ_TIMEOUT))
        .context("ka9q-radio: set read timeout")?;
    Ok(u)
}

fn khz(hz: f64) -> String {
    format!("{:.1} kHz", hz / 1000.0)
}

fn secs(d: Duration) -> String {
    format!("{}", d.as_secs_f64())
}

fn describe(s: &ChannelStatus) -> String {
    format!(
        "ssrc {} ({}, {} Hz, {})",
        s.ssrc.unwrap_or(0),
        khz(s.radio_frequency_hz.unwrap_or(0.0)),
        s.sample_rate_hz.unwrap_or(0),
        wire::encoding_name(s.encoding.unwrap_or(0))
    )
}

/// Whether `rate` is 93.75 Hz times 2^k with k >= 2, the channelizer's
/// table rule (`manta-dsp`'s `Channelizer::new`), checked here so the
/// message can name radiod's `samprate`.
fn rate_is_channelizer_shaped(rate: u64) -> bool {
    // 93.75 = 375 / 4, so rate = 375 * 2^k / 4.
    let x = rate.saturating_mul(4);
    x.is_multiple_of(375) && {
        let n = x / 375;
        n >= 4 && n.is_power_of_two()
    }
}

/// D6: the chosen channel's status must describe a stream manta can read.
fn validate(s: &ChannelStatus, port: u16) -> Result<Encoding> {
    let ssrc = s.ssrc.unwrap_or(0);
    let ch = s.channels.unwrap_or(0);
    if ch != 2 {
        bail!(
            "ka9q-radio channel ssrc {ssrc} is not an IQ stream: it has {ch} output channel{} (IQ needs 2); use mode = iq in its radiod section",
            if ch == 1 { "" } else { "s" }
        );
    }
    if s.envelope == Some(true) {
        bail!("ka9q-radio channel ssrc {ssrc} sends its envelope, not Q, on its second channel; use mode = iq (envelope = no) in its radiod section");
    }
    if s.isb == Some(true) {
        bail!("ka9q-radio channel ssrc {ssrc} is in independent-sideband mode, so its two channels are LSB and USB, not I and Q; use mode = iq in its radiod section");
    }
    let ev = s.encoding.unwrap_or(0);
    let Some(enc) = Encoding::from_wire(ev) else {
        bail!(
            "ka9q-radio channel ssrc {ssrc} uses {} encoding, which manta cannot read; set encoding = f32le or s16be in its radiod section",
            wire::encoding_name(ev)
        );
    };
    let rate = s.sample_rate_hz.unwrap_or(0);
    if rate as f64 > MAX_IQ_WAV_RATE_HZ {
        bail!(
            "ka9q-radio channel ssrc {ssrc} declares {rate} Hz; manta accepts at most {} Hz",
            MAX_IQ_WAV_RATE_HZ as u64
        );
    }
    if !rate_is_channelizer_shaped(rate) {
        bail!("ka9q-radio channel ssrc {ssrc} runs at {rate} Hz; manta needs 93.75 Hz times a power of two (12000, 24000, 48000, 96000, 192000, 384000 ...); set samprate in its radiod section");
    }
    let f = s.radio_frequency_hz.unwrap_or(0.0);
    if !f.is_finite() || f <= 0.0 {
        bail!(
            "ka9q-radio channel ssrc {ssrc} reports no tuned frequency ({} Hz); tune it in radiod first",
            if f.is_finite() { f.round() } else { 0.0 }
        );
    }
    if let Some(d) = s.data_dest {
        if d.port() != port {
            bail!(
                "ka9q-radio channel ssrc {ssrc} sends its data to {d}, not port {port}; set --ka9q-port/input.port to {}",
                d.port()
            );
        }
    }
    Ok(enc)
}

/// One radiod IQ channel as an [`IqSource`].
pub struct Ka9qIqSource {
    data: UdpSocket,
    status_sock: UdpSocket,
    group_label: String,
    port: u16,
    status: ChannelStatus,
    ssrc: u32,
    enc: Encoding,
    fs: f64,
    center_hz: f64,
    queue: VecDeque<Complex32>,
    seq: Option<SeqTracker>,
    recv_buf: Vec<u8>,
    last_accepted: Instant,
    stall_timeout: Duration,
    confirmed_live: Arc<AtomicBool>,
    counters: Arc<InputHealthCounters>,
}

impl Ka9qIqSource {
    /// Join the channel's group, wait for its status, validate it.
    pub fn open(cfg: Ka9qConfig) -> Result<Self> {
        let group = resolve_group(&cfg.group, cfg.port)?;
        // Both sockets join before the status wait, so RTP queues in the
        // kernel meanwhile.
        let data = join_socket(group, cfg.port, cfg.interface)?;
        let status_sock = join_socket(group, cfg.status_port, cfg.interface)?;
        let status = wait_for_status(&status_sock, &cfg)?;
        let enc = validate(&status, cfg.port)?;
        status_sock
            .set_nonblocking(true)
            .context("ka9q-radio: set status socket non-blocking")?;
        Ok(Self {
            data,
            status_sock,
            group_label: cfg.group.clone(),
            port: cfg.port,
            ssrc: status.ssrc.unwrap_or(0),
            fs: status.sample_rate_hz.unwrap_or(0) as f64,
            center_hz: status.radio_frequency_hz.unwrap_or(0.0),
            status,
            enc,
            queue: VecDeque::new(),
            seq: None,
            recv_buf: vec![0u8; RECV_BUF_LEN],
            last_accepted: Instant::now(),
            stall_timeout: cfg.stall_timeout,
            confirmed_live: Arc::new(AtomicBool::new(false)),
            counters: Arc::new(InputHealthCounters::new()),
        })
    }

    /// The SSRC of the channel being read.
    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }

    /// Whether the channel's status reported AGC on (D18's warning).
    pub fn agc_on(&self) -> bool {
        self.status.agc == Some(true)
    }

    /// D13: a retune or format change in the channel's status ends `read`.
    fn check_status(&self, s: &ChannelStatus) -> Result<()> {
        let old = &self.status;
        let ssrc = self.ssrc;
        if let Some(f) = s.radio_frequency_hz {
            if (f - self.center_hz).abs() > 1e-3 {
                bail!(
                    "ka9q-radio channel ssrc {ssrc} retuned from {} to {}",
                    khz(self.center_hz),
                    khz(f)
                );
            }
        }
        let changed = |what: &str, a: Option<String>, b: Option<String>| -> Result<()> {
            if let (Some(a), Some(b)) = (a, b) {
                if a != b {
                    bail!("ka9q-radio channel ssrc {ssrc} changed its {what} from {a} to {b}");
                }
            }
            Ok(())
        };
        let n = |v: Option<u64>| v.map(|x| x.to_string());
        changed("sample rate", n(old.sample_rate_hz), n(s.sample_rate_hz))?;
        changed(
            "encoding",
            old.encoding.map(wire::encoding_name),
            s.encoding.map(wire::encoding_name),
        )?;
        changed("output channel count", n(old.channels), n(s.channels))?;
        changed(
            "RTP payload type",
            old.rtp_pt.map(|p| p.to_string()),
            s.rtp_pt.map(|p| p.to_string()),
        )?;
        Ok(())
    }

    fn drain_status(&mut self) -> Result<()> {
        loop {
            match self.status_sock.recv(&mut self.recv_buf) {
                Ok(n) => match wire::parse_status_packet(&self.recv_buf[..n]) {
                    Ok(Some(s)) if s.ssrc == Some(self.ssrc) => self.check_status(&s)?,
                    Ok(_) => {}
                    Err(_) => self.counters.record_malformed(),
                },
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return Ok(())
                }
                Err(e) => return Err(e).context("ka9q-radio: status socket"),
            }
        }
    }

    fn accept_rtp(&mut self, n: usize) {
        let Ok((h, payload)) = wire::parse_rtp(&self.recv_buf[..n]) else {
            self.counters.record_malformed();
            return;
        };
        if h.ssrc != self.ssrc {
            return; // another channel on a shared group: normal, not counted
        }
        if self.status.rtp_pt.is_some_and(|pt| pt != h.payload_type) {
            self.counters.record_malformed();
            return;
        }
        let before = self.queue.len();
        // An empty payload carries no samples; accepting it would reset
        // the stall deadline and mark the source live with nothing to read.
        let Ok(got @ 1..) = wire::decode_iq(self.enc, payload, &mut self.queue) else {
            self.counters.record_malformed();
            return;
        };
        let fs = self.fs;
        let seq = self.seq.get_or_insert_with(|| {
            let per = (fs * SEQ_WINDOW_S / got.max(1) as f64).min(32767.0);
            SeqTracker::new(per as u16)
        });
        match seq.observe(h.seq) {
            SeqObservation::Stale => {
                self.queue.truncate(before);
                return;
            }
            SeqObservation::Gap { missing } => {
                self.counters.record_gap();
                self.counters.record_dropped(u64::from(missing));
            }
            SeqObservation::Baseline | SeqObservation::InOrder | SeqObservation::Resync => {}
        }
        self.last_accepted = Instant::now();
        self.confirmed_live.store(true, Ordering::Relaxed);
    }
}

fn wait_for_status(sock: &UdpSocket, cfg: &Ka9qConfig) -> Result<ChannelStatus> {
    let start = Instant::now();
    let mut seen: BTreeMap<u32, ChannelStatus> = BTreeMap::new();
    let mut more = false;
    let mut first_at: Option<Instant> = None;
    let mut buf = vec![0u8; RECV_BUF_LEN];
    loop {
        if let (None, Some(t)) = (cfg.ssrc, first_at) {
            if t.elapsed() >= cfg.settle {
                break;
            }
        }
        if start.elapsed() >= cfg.open_timeout {
            break;
        }
        match sock.recv(&mut buf) {
            Ok(n) => {
                let Ok(Some(s)) = wire::parse_status_packet(&buf[..n]) else {
                    continue;
                };
                let Some(id) = s.ssrc else { continue };
                if cfg.ssrc == Some(id) {
                    return Ok(s);
                }
                first_at.get_or_insert_with(Instant::now);
                if seen.len() < MAX_SEEN_CHANNELS || seen.contains_key(&id) {
                    seen.insert(id, s);
                } else {
                    more = true;
                }
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => return Err(e).context("ka9q-radio: status socket"),
        }
    }
    let group = &cfg.group;
    if let Some(want) = cfg.ssrc {
        if seen.is_empty() {
            bail!(no_status_message(cfg));
        }
        let list: Vec<String> = seen
            .values()
            .map(|s| {
                format!(
                    "ssrc {} ({})",
                    s.ssrc.unwrap_or(0),
                    khz(s.radio_frequency_hz.unwrap_or(0.0))
                )
            })
            .collect();
        bail!(
            "no ka9q-radio status for ssrc {want} on {group} within {} s; channels seen on this group: {}; check --ka9q-ssrc/input.ssrc",
            secs(cfg.open_timeout),
            list.join(", ")
        );
    }
    match seen.len() {
        0 => bail!(no_status_message(cfg)),
        1 => Ok(seen.into_values().next().expect("one channel")),
        n => {
            let list: Vec<String> = seen.values().map(describe).collect();
            bail!(
                "{group} carries {n}{} ka9q-radio channels; choose one with --ka9q-ssrc or input.ssrc: {}",
                if more { " or more" } else { "" },
                list.join(", ")
            )
        }
    }
}

fn no_status_message(cfg: &Ka9qConfig) -> String {
    format!(
        "no ka9q-radio status from {} (port {}) within {} s: check that radiod is running with a channel sending to this group, that its ttl is at least 1 if radiod runs on another machine (ttl = 0, radiod's default, keeps traffic on that machine), and that --ka9q-interface/input.interface names the interface that reaches it",
        cfg.group,
        cfg.status_port,
        secs(cfg.open_timeout)
    )
}

impl IqSource for Ka9qIqSource {
    fn sample_rate(&self) -> f64 {
        self.fs
    }

    fn center_freq_hz(&self) -> f64 {
        self.center_hz
    }

    /// D14: the channel's filter edges, clamped to the Nyquist span.
    fn rf_passband_hz(&self) -> (f64, f64) {
        let half = self.fs / 2.0;
        match (self.status.low_edge_hz, self.status.high_edge_hz) {
            (Some(lo), Some(hi)) if lo.is_finite() && hi.is_finite() && lo < hi => {
                let (lo, hi) = (lo.max(-half), hi.min(half));
                if lo < hi {
                    (lo, hi)
                } else {
                    (-half, half)
                }
            }
            _ => (-half, half),
        }
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            self.drain_status()?;
            if !self.queue.is_empty() {
                let n = buf.len().min(self.queue.len());
                for (dst, src) in buf.iter_mut().zip(self.queue.drain(..n)) {
                    *dst = src;
                }
                return Ok(n);
            }
            if self.last_accepted.elapsed() >= self.stall_timeout {
                bail!(
                    "ka9q-radio stream stalled: no RTP data for ssrc {} on {}:{} for {} s",
                    self.ssrc,
                    self.group_label,
                    self.port,
                    secs(self.stall_timeout)
                );
            }
            match self.data.recv(&mut self.recv_buf) {
                Ok(n) => self.accept_rtp(n),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(e) => return Err(e).context("ka9q-radio: data socket"),
            }
        }
    }

    fn confirmed_live_handle(&self) -> Option<Arc<AtomicBool>> {
        Some(self.confirmed_live.clone())
    }

    fn health_counters(&self) -> Option<Arc<InputHealthCounters>> {
        Some(self.counters.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn group_literal_must_be_ipv4_multicast() {
        assert!(check_group("239.101.6.1").is_ok());
        assert!(check_group("224.0.0.251").is_ok());
        assert!(check_group("cw-20m.local").is_ok());
        let e = check_group("192.168.1.5").unwrap_err();
        assert_eq!(
            e,
            "must be an IPv4 multicast address (224.0.0.0 to 239.255.255.255) or a host name, got 192.168.1.5"
        );
        assert!(check_group("<your-radiod-data-group>").is_err());
        assert!(check_group("").is_err());
        assert!(check_group("a..b").is_err());
    }

    #[test]
    fn ipv6_group_is_refused() {
        assert_eq!(
            check_group("ff02::1").unwrap_err(),
            "IPv6 groups are not supported yet; give radiod's IPv4 data group, got ff02::1"
        );
    }

    #[test]
    fn join_plan_tolerates_a_failed_default_join_when_loopback_joined() {
        let any = Ipv4Addr::UNSPECIFIED;
        let lo = Ipv4Addr::LOCALHOST;
        let eth: Ipv4Addr = "192.168.1.20".parse().unwrap();
        assert_eq!(join_interfaces(None), vec![any, lo]);
        assert_eq!(join_interfaces(Some(eth)), vec![eth, lo]);
        assert_eq!(join_interfaces(Some(lo)), vec![lo]);
        let fail = || Err("EADDRINUSE".to_string());
        assert!(check_joins(None, &[(any, fail()), (lo, Ok(()))]).is_ok());
        assert!(check_joins(None, &[(any, Ok(())), (lo, fail())]).is_ok());
        assert!(check_joins(None, &[(any, fail()), (lo, fail())]).is_err());
        let e = check_joins(Some(eth), &[(eth, fail()), (lo, Ok(()))]).unwrap_err();
        assert!(e.contains("192.168.1.20"), "{e}");
        assert!(check_joins(Some(eth), &[(eth, Ok(())), (lo, fail())]).is_ok());
    }

    #[test]
    fn rate_shape_matches_the_channelizer_rule() {
        for r in [375, 12_000, 48_000, 96_000, 192_000, 384_000, 6_144_000] {
            assert!(rate_is_channelizer_shaped(r), "{r}");
        }
        for r in [0, 16_000, 44_100, 187, 100_000] {
            assert!(!rate_is_channelizer_shaped(r), "{r}");
        }
    }

    #[test]
    #[ignore = "needs a live radiod: set MANTA_TEST_KA9Q_GROUP (and optionally MANTA_TEST_KA9Q_SSRC)"]
    fn reads_a_live_radiod_channel() {
        let group = std::env::var("MANTA_TEST_KA9Q_GROUP").expect("MANTA_TEST_KA9Q_GROUP");
        let mut cfg = Ka9qConfig::new(group);
        cfg.ssrc = std::env::var("MANTA_TEST_KA9Q_SSRC")
            .ok()
            .map(|s| s.parse().expect("MANTA_TEST_KA9Q_SSRC is a u32"));
        let mut src = Ka9qIqSource::open(cfg).expect("open");
        let want = src.sample_rate() as usize;
        let mut buf = vec![Complex32::new(0.0, 0.0); 4096];
        let mut got = 0;
        while got < want {
            got += src.read(&mut buf).expect("read");
        }
        assert!(src.confirmed_live_handle().unwrap().load(Ordering::Relaxed));
    }

    // ---- Socket tests against a fake radiod on loopback multicast (D19).

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    mod sockets {
        use super::*;
        use std::sync::atomic::AtomicU8;
        use std::thread::JoinHandle;
        use wire::RtpHeader;

        static NEXT: AtomicU8 = AtomicU8::new(1);

        fn unique_group() -> Ipv4Addr {
            let pid = std::process::id();
            Ipv4Addr::new(
                239,
                (pid % 250) as u8 + 1,
                (pid / 250 % 250) as u8,
                NEXT.fetch_add(1, Ordering::Relaxed),
            )
        }

        fn free_port() -> u16 {
            UdpSocket::bind("0.0.0.0:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port()
        }

        fn sender() -> UdpSocket {
            let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
            s.set_multicast_if_v4(&Ipv4Addr::LOCALHOST).unwrap();
            s.set_multicast_ttl_v4(0).unwrap();
            s.set_multicast_loop_v4(true).unwrap();
            s.bind(&SockAddr::from(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)))
                .unwrap();
            s.into()
        }

        /// Sends every status in `statuses` every 100 ms until dropped.
        struct FakeRadiod {
            group: Ipv4Addr,
            data_port: u16,
            status_port: u16,
            statuses: Arc<Mutex<Vec<ChannelStatus>>>,
            stop: Arc<AtomicBool>,
            thread: Option<JoinHandle<()>>,
            tx: UdpSocket,
        }

        impl FakeRadiod {
            fn start(statuses: Vec<ChannelStatus>) -> Self {
                let group = unique_group();
                let data_port = free_port();
                let status_port = free_port();
                let statuses = Arc::new(Mutex::new(
                    statuses
                        .into_iter()
                        .map(|mut s| {
                            s.data_dest
                                .get_or_insert(std::net::SocketAddr::V4(SocketAddrV4::new(
                                    group, data_port,
                                )));
                            s
                        })
                        .collect::<Vec<_>>(),
                ));
                let stop = Arc::new(AtomicBool::new(false));
                let (st, sp) = (statuses.clone(), stop.clone());
                let thread = std::thread::spawn(move || {
                    let tx = sender();
                    while !sp.load(Ordering::Relaxed) {
                        let list = st.lock().unwrap().clone();
                        for s in &list {
                            let _ = tx.send_to(
                                &wire::encode_status_packet(s),
                                SocketAddrV4::new(group, status_port),
                            );
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                });
                Self {
                    group,
                    data_port,
                    status_port,
                    statuses,
                    stop,
                    thread: Some(thread),
                    tx: sender(),
                }
            }

            fn config(&self, ssrc: Option<u32>) -> Ka9qConfig {
                let mut c = Ka9qConfig::new(self.group.to_string());
                c.port = self.data_port;
                c.status_port = self.status_port;
                c.ssrc = ssrc;
                c.open_timeout = Duration::from_millis(800);
                c.settle = Duration::from_millis(350);
                c.stall_timeout = Duration::from_millis(600);
                c
            }

            fn send(&self, datagram: &[u8]) {
                self.tx
                    .send_to(datagram, SocketAddrV4::new(self.group, self.data_port))
                    .unwrap();
            }

            fn send_rtp(&self, ssrc: u32, seq: u16, enc: Encoding, samples: &[Complex32]) {
                let h = RtpHeader {
                    payload_type: 111,
                    seq,
                    timestamp: u32::from(seq) * samples.len() as u32,
                    ssrc,
                    marker: false,
                };
                self.send(&wire::encode_rtp(&h, &wire::encode_iq(enc, samples)));
            }
        }

        impl Drop for FakeRadiod {
            fn drop(&mut self) {
                self.stop.store(true, Ordering::Relaxed);
                if let Some(t) = self.thread.take() {
                    let _ = t.join();
                }
            }
        }

        fn channel(ssrc: u32, freq: f64) -> ChannelStatus {
            ChannelStatus {
                ssrc: Some(ssrc),
                sample_rate_hz: Some(96_000),
                radio_frequency_hz: Some(freq),
                low_edge_hz: Some(-45_000.0),
                high_edge_hz: Some(45_000.0),
                channels: Some(2),
                isb: Some(false),
                envelope: Some(false),
                agc: Some(false),
                rtp_pt: Some(111),
                encoding: Some(Encoding::F32Le.wire_value()),
                data_dest: None,
            }
        }

        const SSRC: u32 = 14_000_000;

        fn ramp(n: usize, k: usize) -> Vec<Complex32> {
            (0..n)
                .map(|i| {
                    let x = ((i + k) % 200) as f32 / 256.0 - 0.4;
                    Complex32::new(x, -x * 0.5)
                })
                .collect()
        }

        fn read_n(src: &mut Ka9qIqSource, n: usize) -> Vec<Complex32> {
            let mut out = Vec::new();
            let mut buf = vec![Complex32::new(0.0, 0.0); 4096];
            while out.len() < n {
                let k = src.read(&mut buf).expect("read");
                out.extend_from_slice(&buf[..k]);
            }
            out
        }

        fn open_err(s: ChannelStatus) -> String {
            let f = FakeRadiod::start(vec![s]);
            match Ka9qIqSource::open(f.config(Some(SSRC))) {
                Ok(_) => panic!("open should fail"),
                Err(e) => format!("{e:#}"),
            }
        }

        #[test]
        fn open_learns_rate_encoding_and_frequency_from_the_status_packet() {
            let f = FakeRadiod::start(vec![channel(SSRC, 14_000_000.0)]);
            let src = Ka9qIqSource::open(f.config(Some(SSRC))).unwrap();
            assert_eq!(src.sample_rate(), 96_000.0);
            assert_eq!(src.center_freq_hz(), 14_000_000.0);
            assert_eq!(src.enc, Encoding::F32Le);
            assert!(!src.agc_on());
        }

        #[test]
        fn open_picks_the_only_channel_when_no_ssrc_is_configured() {
            let f = FakeRadiod::start(vec![channel(SSRC, 14_000_000.0)]);
            let src = Ka9qIqSource::open(f.config(None)).unwrap();
            assert_eq!(src.ssrc(), SSRC);
        }

        #[test]
        fn open_refuses_a_group_with_several_channels_and_lists_them() {
            let f = FakeRadiod::start(vec![
                channel(7_074_000, 7_074_000.0),
                channel(14_074_000, 14_074_000.0),
            ]);
            let e = format!("{:#}", Ka9qIqSource::open(f.config(None)).err().unwrap());
            assert!(e.contains("carries 2 ka9q-radio channels"), "{e}");
            assert!(
                e.contains("ssrc 7074000 (7074.0 kHz, 96000 Hz, f32le)"),
                "{e}"
            );
            assert!(e.contains("ssrc 14074000 (14074.0 kHz"), "{e}");
        }

        #[test]
        fn open_remembers_at_most_max_seen_channels() {
            let n = MAX_SEEN_CHANNELS as u32 + 6;
            let f = FakeRadiod::start(
                (0..n)
                    .map(|i| channel(7_000_000 + i, 7_000_000.0 + f64::from(i)))
                    .collect(),
            );
            let e = format!("{:#}", Ka9qIqSource::open(f.config(None)).err().unwrap());
            assert!(
                e.contains(&format!(
                    "carries {MAX_SEEN_CHANNELS} or more ka9q-radio channels"
                )),
                "{e}"
            );
            assert_eq!(e.matches(" Hz, f32le)").count(), MAX_SEEN_CHANNELS, "{e}");
        }

        #[test]
        fn open_with_ssrc_reads_only_that_channel() {
            let (a, b) = (7_074_000, 14_074_000);
            let f = FakeRadiod::start(vec![channel(a, 7_074_000.0), channel(b, 14_074_000.0)]);
            let mut src = Ka9qIqSource::open(f.config(Some(b))).unwrap();
            assert_eq!(src.center_freq_hz(), 14_074_000.0);
            let want = ramp(180, 0);
            f.send_rtp(a, 1, Encoding::F32Le, &ramp(180, 77));
            f.send_rtp(b, 1, Encoding::F32Le, &want);
            assert_eq!(read_n(&mut src, 180), want);
            assert_eq!(src.counters.malformed_packets(), 0);
        }

        #[test]
        fn open_times_out_naming_ttl_when_no_status_arrives() {
            let f = FakeRadiod::start(vec![]);
            let e = format!("{:#}", Ka9qIqSource::open(f.config(None)).err().unwrap());
            assert!(e.contains("no ka9q-radio status from"), "{e}");
            assert!(e.contains("ttl"), "{e}");
        }

        #[test]
        fn open_rejects_a_non_iq_channel() {
            let mut s = channel(SSRC, 14e6);
            s.channels = Some(1);
            assert!(
                open_err(s).contains("is not an IQ stream: it has 1 output channel (IQ needs 2)")
            );
            let mut s = channel(SSRC, 14e6);
            s.envelope = Some(true);
            assert!(open_err(s).contains("sends its envelope, not Q"));
            let mut s = channel(SSRC, 14e6);
            s.isb = Some(true);
            assert!(open_err(s).contains("independent-sideband mode"));
        }

        #[test]
        fn open_rejects_an_unreadable_encoding_naming_it() {
            let mut s = channel(SSRC, 14e6);
            s.encoding = Some(3);
            assert!(open_err(s).contains("uses opus encoding"));
        }

        #[test]
        fn open_rejects_a_rate_the_channelizer_cannot_take() {
            let mut s = channel(SSRC, 14e6);
            s.sample_rate_hz = Some(16_000);
            assert!(
                open_err(s).contains("runs at 16000 Hz; manta needs 93.75 Hz times a power of two")
            );
        }

        #[test]
        fn open_rejects_a_rate_above_the_ceiling() {
            let mut s = channel(SSRC, 14e6);
            s.sample_rate_hz = Some(400_000_000);
            assert!(
                open_err(s).contains("declares 400000000 Hz; manta accepts at most 10000000 Hz")
            );
        }

        #[test]
        fn open_rejects_a_status_that_names_another_data_port() {
            let mut s = channel(SSRC, 14e6);
            s.data_dest = Some("239.1.1.1:5010".parse().unwrap());
            assert!(open_err(s).contains("set --ka9q-port/input.port to 5010"));
        }

        #[test]
        fn open_rejects_a_channel_with_no_frequency() {
            let mut s = channel(SSRC, 14e6);
            s.radio_frequency_hz = Some(0.0);
            assert!(open_err(s).contains("reports no tuned frequency (0 Hz)"));
        }

        #[test]
        fn read_returns_the_sent_samples_for_every_encoding() {
            for enc in [
                Encoding::S16Le,
                Encoding::S16Be,
                Encoding::F16Le,
                Encoding::F16Be,
                Encoding::F32Le,
                Encoding::F32Be,
            ] {
                let mut s = channel(SSRC, 14e6);
                s.encoding = Some(enc.wire_value());
                let f = FakeRadiod::start(vec![s]);
                let mut src = Ka9qIqSource::open(f.config(Some(SSRC))).unwrap();
                // 1 s of 96 kS/s, 180 samples per packet, read in batches
                // so a burst never outruns the kernel's receive buffer.
                let sent: Vec<Complex32> = (0..533).flat_map(|k| ramp(180, k)).collect();
                let mut got = Vec::new();
                for (b, batch) in sent.chunks(180 * 40).enumerate() {
                    for (k, chunk) in batch.chunks(180).enumerate() {
                        f.send_rtp(SSRC, (b * 40 + k) as u16, enc, chunk);
                    }
                    got.extend(read_n(&mut src, batch.len()));
                }
                if matches!(enc, Encoding::F32Le | Encoding::F32Be) {
                    assert_eq!(got, sent, "{enc:?}");
                } else {
                    for (a, b) in sent.iter().zip(&got) {
                        assert!((a - b).norm() < 2e-3, "{enc:?}: {a} vs {b}");
                    }
                }
            }
        }

        #[test]
        fn confirmed_live_starts_false_and_flips_on_the_first_rtp_packet() {
            let f = FakeRadiod::start(vec![channel(SSRC, 14e6)]);
            let mut src = Ka9qIqSource::open(f.config(Some(SSRC))).unwrap();
            let live = src.confirmed_live_handle().unwrap();
            assert!(!live.load(Ordering::Relaxed));
            f.send_rtp(SSRC, 9, Encoding::F32Le, &ramp(180, 0));
            read_n(&mut src, 180);
            assert!(live.load(Ordering::Relaxed));
        }

        #[test]
        fn a_skipped_sequence_number_counts_one_gap_and_one_dropped_packet() {
            let f = FakeRadiod::start(vec![channel(SSRC, 14e6)]);
            let mut src = Ka9qIqSource::open(f.config(Some(SSRC))).unwrap();
            for seq in [1u16, 2, 4] {
                f.send_rtp(SSRC, seq, Encoding::F32Le, &ramp(180, 0));
            }
            read_n(&mut src, 540);
            let c = src.health_counters().unwrap();
            assert_eq!((c.gaps_detected(), c.dropped_packets()), (1, 1));
        }

        #[test]
        fn malformed_datagrams_are_counted_and_reading_continues() {
            let f = FakeRadiod::start(vec![channel(SSRC, 14e6)]);
            let mut src = Ka9qIqSource::open(f.config(Some(SSRC))).unwrap();
            let good = ramp(180, 0);
            let h = |pt: u8, seq: u16| RtpHeader {
                payload_type: pt,
                seq,
                timestamp: 0,
                ssrc: SSRC,
                marker: false,
            };
            f.send(&[0x80, 111, 0, 1]); // short
            let mut v1 = wire::encode_rtp(&h(111, 1), &wire::encode_iq(Encoding::F32Le, &good));
            v1[0] = 0x40; // version 1
            f.send(&v1);
            f.send(&wire::encode_rtp(&h(111, 2), &[0u8; 12])); // odd payload
            f.send(&wire::encode_rtp(
                &h(96, 3),
                &wire::encode_iq(Encoding::F32Le, &good),
            ));
            let nan = [Complex32::new(f32::NAN, 0.0)];
            f.send(&wire::encode_rtp(
                &h(111, 4),
                &wire::encode_iq(Encoding::F32Le, &nan),
            ));
            f.send(&wire::encode_rtp(&h(111, 5), &[])); // no samples
            f.send_rtp(SSRC, 6, Encoding::F32Le, &good);
            assert_eq!(read_n(&mut src, 180), good);
            assert_eq!(src.counters.malformed_packets(), 6);
        }

        #[test]
        fn read_fails_after_the_stall_timeout_even_while_other_channels_flood_the_group() {
            let f = FakeRadiod::start(vec![channel(SSRC, 14e6)]);
            let mut src = Ka9qIqSource::open(f.config(Some(SSRC))).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let flood = {
                let (stop, group, port) = (stop.clone(), f.group, f.data_port);
                std::thread::spawn(move || {
                    let tx = sender();
                    let mut seq = 0u16;
                    let p = wire::encode_iq(Encoding::F32Le, &ramp(180, 0));
                    while !stop.load(Ordering::Relaxed) {
                        seq = seq.wrapping_add(1);
                        let h = RtpHeader {
                            payload_type: 111,
                            seq,
                            timestamp: 0,
                            ssrc: 1,
                            marker: false,
                        };
                        let _ =
                            tx.send_to(&wire::encode_rtp(&h, &p), SocketAddrV4::new(group, port));
                        std::thread::sleep(Duration::from_millis(1));
                    }
                })
            };
            let t = Instant::now();
            let mut buf = vec![Complex32::new(0.0, 0.0); 64];
            let e = format!("{:#}", src.read(&mut buf).err().unwrap());
            stop.store(true, Ordering::Relaxed);
            flood.join().unwrap();
            assert!(
                e.contains("ka9q-radio stream stalled: no RTP data for ssrc 14000000"),
                "{e}"
            );
            assert!(t.elapsed() < Duration::from_secs(5));
        }

        #[test]
        fn a_retune_in_the_status_ends_read_with_both_frequencies_named() {
            let f = FakeRadiod::start(vec![channel(SSRC, 14_000_000.0)]);
            let mut src = Ka9qIqSource::open(f.config(Some(SSRC))).unwrap();
            f.statuses.lock().unwrap()[0].radio_frequency_hz = Some(14_050_000.0);
            std::thread::sleep(Duration::from_millis(250));
            let mut buf = vec![Complex32::new(0.0, 0.0); 64];
            let e = format!("{:#}", src.read(&mut buf).err().unwrap());
            assert!(e.contains("retuned from 14000.0 kHz to 14050.0 kHz"), "{e}");
        }

        #[test]
        fn a_format_change_in_the_status_ends_read() {
            let f = FakeRadiod::start(vec![channel(SSRC, 14e6)]);
            let mut src = Ka9qIqSource::open(f.config(Some(SSRC))).unwrap();
            f.statuses.lock().unwrap()[0].encoding = Some(Encoding::S16Be.wire_value());
            std::thread::sleep(Duration::from_millis(250));
            let mut buf = vec![Complex32::new(0.0, 0.0); 64];
            let e = format!("{:#}", src.read(&mut buf).err().unwrap());
            assert!(
                e.contains("changed its encoding from f32le to s16be"),
                "{e}"
            );
        }

        #[test]
        fn two_sources_read_the_same_group_and_port() {
            let f = FakeRadiod::start(vec![channel(SSRC, 14e6)]);
            let mut a = Ka9qIqSource::open(f.config(Some(SSRC))).unwrap();
            let mut b = Ka9qIqSource::open(f.config(Some(SSRC))).unwrap();
            let sent: Vec<Complex32> = (0..20).flat_map(|k| ramp(180, k)).collect();
            for (k, c) in sent.chunks(180).enumerate() {
                f.send_rtp(SSRC, k as u16, Encoding::F32Le, c);
            }
            assert_eq!(read_n(&mut a, sent.len()), sent);
            assert_eq!(read_n(&mut b, sent.len()), sent);
        }

        #[test]
        fn rf_passband_follows_the_channel_filter_edges() {
            let f = FakeRadiod::start(vec![channel(SSRC, 14e6)]);
            let mut src = Ka9qIqSource::open(f.config(Some(SSRC))).unwrap();
            assert_eq!(src.rf_passband_hz(), (-45_000.0, 45_000.0));
            src.status.low_edge_hz = Some(-60_000.0);
            src.status.high_edge_hz = Some(60_000.0);
            assert_eq!(src.rf_passband_hz(), (-48_000.0, 48_000.0));
            src.status.low_edge_hz = None;
            src.status.high_edge_hz = None;
            assert_eq!(src.rf_passband_hz(), (-48_000.0, 48_000.0));
        }

        #[test]
        fn health_counters_and_liveness_handles_are_shared_arcs() {
            let f = FakeRadiod::start(vec![channel(SSRC, 14e6)]);
            let src = Ka9qIqSource::open(f.config(Some(SSRC))).unwrap();
            assert!(Arc::ptr_eq(
                &src.health_counters().unwrap(),
                &src.health_counters().unwrap()
            ));
            assert!(Arc::ptr_eq(
                &src.confirmed_live_handle().unwrap(),
                &src.confirmed_live_handle().unwrap()
            ));
        }
    }
}
