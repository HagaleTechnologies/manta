//! openHPSDR protocol 2 receive source (MAN-305): ANAN-class radios
//! (Hermes, Angelia, Orion, Orion II, Saturn/G2). One DDC (MAN-307).
//!
//! One unconnected UDP socket (D5) sends the four command streams to the
//! D4 port map and receives every datagram the radio sends back; `read()`
//! demultiplexes by source port, keeps DDC0's IQ, accounts for sequence
//! gaps (D10/D11), drives the keepalive (D8) and bounds stalls (D13).
//! With automatic attenuation on, it also reads the high-priority status
//! datagram and drives ADC0's step attenuator (MAN-308).

pub mod attenuator;
pub mod wire;

use attenuator::{AttenuationChange, AttenuationStep, AutoAttenuation, Controller, Timing};

use super::{
    GapStats, HpsdrConfig, INITIAL_BURST_REPEATS, MAX_CONSECUTIVE_MALFORMED,
    MAX_CONSECUTIVE_TIMEOUTS, READ_TIMEOUT, RECV_BUF_LEN,
};
use crate::{InputHealthCounters, IqSource};
use anyhow::{bail, Context, Result};
use num_complex::Complex32;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::collections::VecDeque;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Requested `SO_RCVBUF` (D5): the ka9q precedent's 4 MiB. A smaller grant
/// is not an error.
const RCVBUF_BYTES: usize = 4 << 20;

/// Datagrams a socket buffer can hold at most (twice the 4 MiB request,
/// which Linux doubles, over a 1444-byte datagram, rounded up): a real loss
/// can be this much larger than the wall clock since the last datagram
/// allows, because that datagram may have waited in the buffer while its
/// successors were dropped.
const MAX_BUFFERED_DATAGRAMS: u64 = 8192;

/// MAN-423: a pause between reads at least this long parks the socket: the
/// radio keeps sending, and what it sends either waits in the receive buffer
/// or, once that is full, is dropped. The radio cannot outrun the 8,192
/// allowance in less (81 datagrams at 192 kS/s), and a decode loop keeping
/// up reads a datagram every 1.24 ms at 192 kS/s.
const PARKED_AFTER: Duration = Duration::from_millis(100);

/// MAN-423: a pause in reading whose backlog may still be draining.
#[derive(Debug, Clone, Copy)]
struct Parked {
    /// When the pause began: the stream's clock is accounted up to here.
    from: Instant,
    /// Samples accounted since `from`: every accepted datagram's 238 plus
    /// whatever it reported lost or estimated for a restart.
    accounted: u64,
    /// Accepted datagrams until the backlog is surely drained.
    left: u64,
}

/// The four rates rx-specific can encode (D14), for the error text.
const ENCODABLE_RATES_TEXT: &str = "48000/96000/192000/384000";

/// Half the 122.88 MHz ADC clock: the phase word's range (D14).
const MAX_CENTRE_FREQ_HZ: f64 = wire::ADC_CLOCK_HZ / 2.0;

/// Highest source port in the D4 map, relative to `port`: DDC0's IQ.
const DDC0_PORT_OFFSET: u16 = 11;

/// Highest radio source port, relative to `port`, that is read and ignored
/// (command response, high-priority status, mic, wideband; D4/D5), except
/// high-priority status when automatic attenuation is on (MAN-308).
const LAST_IGNORED_PORT_OFFSET: u16 = 10;

/// The radio's high-priority status port, relative to `port` (D4; MAN-308).
const STATUS_PORT_OFFSET: u16 = 1;

/// Waits after each bring-up burst (D6: "let the FPGA digest").
const AFTER_GENERAL: Duration = Duration::from_millis(100);
const AFTER_RX_SPECIFIC: Duration = Duration::from_millis(50);
const AFTER_TX_SPECIFIC: Duration = Duration::from_millis(50);

/// Keepalive cadence (D8, piHPSDR's).
const HIGH_PRIORITY_EVERY: Duration = Duration::from_millis(100);
const RX_TX_SPECIFIC_EVERY: Duration = Duration::from_millis(200);
const GENERAL_EVERY: Duration = Duration::from_millis(800);

/// How many times `Drop` sends high-priority with run = 0 (D8).
const STOP_REPEATS: usize = 2;

/// Production stall bound (D13): the same 40 x 250 ms budget as protocol 1,
/// measured as elapsed time since the last valid IQ datagram.
pub(crate) const STALL_AFTER: Duration =
    Duration::from_millis(READ_TIMEOUT.as_millis() as u64 * MAX_CONSECUTIVE_TIMEOUTS as u64);

/// The four host -> radio command streams, each with its own sequence
/// counter from 0 (D6). The value is the offset from `port` (D4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stream {
    General = 0,
    RxSpecific = 1,
    TxSpecific = 2,
    HighPriority = 3,
}

impl Stream {
    /// The stream's name in error text.
    fn name(self) -> &'static str {
        match self {
            Stream::General => "general",
            Stream::RxSpecific => "rx-specific",
            Stream::TxSpecific => "tx-specific",
            Stream::HighPriority => "high-priority",
        }
    }
}

/// What `read()` does with a `recv_from` error (D5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecvErrorAction {
    /// Windows reports an earlier ICMP port-unreachable on an unconnected
    /// UDP socket as `ConnectionReset`; it must not end the stream.
    Retry,
    /// The 250 ms read timeout: check the stall bound and read again.
    Timeout,
    /// A real I/O error.
    Fatal,
}

/// Classify a `recv_from` error (D5).
pub(crate) fn classify_recv_error(e: &io::Error) -> RecvErrorAction {
    match e.kind() {
        io::ErrorKind::ConnectionReset => RecvErrorAction::Retry,
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted => {
            RecvErrorAction::Timeout
        }
        _ => RecvErrorAction::Fatal,
    }
}

/// Protocol-2 validation (D14): one DDC (MAN-307), an encodable rate, and
/// a whole-Hz centre frequency below the ADC Nyquist limit. Returns the
/// rate in kHz and the centre frequency.
pub(crate) fn validate_protocol2(cfg: &HpsdrConfig) -> Result<(u16, f64)> {
    if cfg.ddc_count != 1 {
        bail!(
            "HPSDR protocol 2 supports exactly one DDC, got ddc_count = {}; more than one \
             protocol-2 DDC is MAN-307",
            cfg.ddc_count
        );
    }
    if cfg.center_freq_hz.len() != 1 {
        bail!(
            "HPSDR protocol 2 needs exactly one centre frequency for its one DDC, got {}",
            cfg.center_freq_hz.len()
        );
    }
    let Some(rate_khz) = wire::rate_khz(cfg.sample_rate_hz) else {
        bail!(
            "HPSDR protocol 2 only encodes exactly {ENCODABLE_RATES_TEXT} Hz sample rates, got {}",
            cfg.sample_rate_hz
        );
    };
    let f = cfg.center_freq_hz[0];
    if !f.is_finite() || f <= 0.0 || f.fract() != 0.0 {
        bail!("HPSDR protocol 2: centre frequency {f} Hz is not a positive whole number of Hz");
    }
    if f >= MAX_CENTRE_FREQ_HZ {
        bail!(
            "HPSDR protocol 2: centre frequency {f} Hz is not below {MAX_CENTRE_FREQ_HZ} Hz, the \
             radio's 122.88 MHz ADC Nyquist limit"
        );
    }
    if u32::from(cfg.port) + u32::from(DDC0_PORT_OFFSET) > u32::from(u16::MAX) {
        bail!(
            "HPSDR protocol 2: port {} leaves no room for the radio's port map (port to port + {})",
            cfg.port,
            DDC0_PORT_OFFSET
        );
    }
    Ok((rate_khz, f))
}

/// MAN-308: one open's automatic attenuation, the shared handle plus the
/// controller that decides each step.
struct Attenuation {
    handle: Arc<AutoAttenuation>,
    controller: Controller,
}

/// A protocol-2 radio's DDC0 as an [`IqSource`].
pub struct Protocol2IqSource {
    socket: UdpSocket,
    host: String,
    radio_ip: Ipv4Addr,
    port: u16,
    board: wire::BoardInfo,
    rate_khz: u16,
    phase_word: u32,
    alex0: u32,
    alex1: u32,
    /// Next sequence number per [`Stream`] (D6).
    seq: [u32; 4],
    last_high_priority: Instant,
    last_rx_tx_specific: Instant,
    last_general: Instant,
    tracker: wire::SeqTracker,
    /// Samples of the datagram being handed out, from `queue_pos` on.
    queue: Vec<Complex32>,
    queue_pos: usize,
    pending_missing: Option<u64>,
    pending_discontinuity: Option<u64>,
    /// Time of the last valid IQ datagram (or of the open), stale ones
    /// included: the stall clock (D13).
    last_valid_iq: Instant,
    /// Time of the last IQ datagram the tracker accepted (or of the open):
    /// the restart estimate and the plausible-gap bound (D10/D11). Stale
    /// datagrams do not move it, so a restart after stale ones still
    /// measures the whole outage.
    last_accepted_iq: Instant,
    /// MAN-423: when the last `read()` returned (or the open's run = 1):
    /// a longer pause than [`PARKED_AFTER`] before the next opens `parked`.
    read_returned: Instant,
    /// MAN-423: the pause whose backlog may still be draining, if any.
    parked: Option<Parked>,
    /// MAN-423: accepted datagrams a pause's window lasts (one full socket
    /// buffer, [`MAX_BUFFERED_DATAGRAMS`]).
    pub(crate) park_window: u64,
    /// MAN-423 review: datagrams already received by a restart's look-ahead
    /// ([`Self::restart_outage`]), handled before the socket's next.
    look_ahead: VecDeque<(Vec<u8>, SocketAddr)>,
    /// MAN-259: offset from entering `open()` to immediately before the
    /// first run command; [`super::open_receiver`] adds its discovery time.
    pub(crate) acquisition_start_offset: Duration,
    consecutive_malformed: u32,
    recv_buf: Box<[u8]>,
    sample_rate_hz: f64,
    center_freq_hz: f64,
    confirmed_live: Arc<AtomicBool>,
    health_counters: Arc<InputHealthCounters>,
    /// Elapsed time with no valid IQ datagram before `read()` errors (D13).
    pub(crate) stall_after: Duration,
    /// Consecutive malformed datagrams before `read()` errors (D13).
    pub(crate) max_consecutive_malformed: u32,
    /// MAN-308: `Some` with `input.auto_attenuation`.
    attenuation: Option<Attenuation>,
}

impl Protocol2IqSource {
    /// Validate `cfg` for protocol 2, bring the radio up and return DDC0.
    pub fn open(cfg: &HpsdrConfig, board: wire::BoardInfo) -> Result<Self> {
        let entered = Instant::now();
        let (rate_khz, freq_hz) = validate_protocol2(cfg)?;
        let radio_ip = (cfg.host.as_str(), cfg.port)
            .to_socket_addrs()
            .with_context(|| format!("resolve HPSDR protocol 2 radio {}", cfg.host))?
            .find_map(|a| match a {
                SocketAddr::V4(v4) => Some(*v4.ip()),
                SocketAddr::V6(_) => None,
            })
            .with_context(|| format!("HPSDR protocol 2 radio {} has no IPv4 address", cfg.host))?;

        let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
            .context("create HPSDR protocol 2 UDP socket")?;
        let _ = s.set_recv_buffer_size(RCVBUF_BYTES);
        s.bind(&SockAddr::from(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)))
            .context("bind HPSDR protocol 2 UDP socket")?;
        s.set_read_timeout(Some(READ_TIMEOUT))
            .context("set HPSDR protocol 2 UDP read timeout")?;
        let socket: UdpSocket = s.into();

        let (alex0, alex1) = wire::alex_words(board.scheme, freq_hz);
        let now = Instant::now();
        let mut src = Self {
            socket,
            host: cfg.host.clone(),
            radio_ip,
            port: cfg.port,
            board,
            rate_khz,
            phase_word: wire::phase_word(freq_hz),
            alex0,
            alex1,
            seq: [0; 4],
            last_high_priority: now,
            last_rx_tx_specific: now,
            last_general: now,
            tracker: wire::SeqTracker::new(),
            queue: Vec::with_capacity(wire::SAMPLES_PER_DATAGRAM),
            queue_pos: 0,
            pending_missing: None,
            pending_discontinuity: None,
            last_valid_iq: now,
            last_accepted_iq: now,
            read_returned: now,
            parked: None,
            park_window: MAX_BUFFERED_DATAGRAMS,
            look_ahead: VecDeque::new(),
            acquisition_start_offset: Duration::ZERO,
            consecutive_malformed: 0,
            recv_buf: vec![0u8; RECV_BUF_LEN].into_boxed_slice(),
            sample_rate_hz: cfg.sample_rate_hz,
            center_freq_hz: freq_hz,
            confirmed_live: Arc::new(AtomicBool::new(false)),
            health_counters: Arc::new(InputHealthCounters::new()),
            stall_after: STALL_AFTER,
            max_consecutive_malformed: MAX_CONSECUTIVE_MALFORMED,
            // Engaged before the bring-up sends, so a reopen's restored
            // level goes out with run = 1 (MAN-308 D6).
            attenuation: cfg.auto_attenuation.as_ref().map(|h| Attenuation {
                controller: Controller::new(h.engage(), now, Timing::default()),
                handle: h.clone(),
            }),
        };

        // D6 order: general, rx-specific, tx-specific, each twice with the
        // FPGA's digest waits, then run = 1 twice.
        for (stream, wait) in [
            (Stream::General, AFTER_GENERAL),
            (Stream::RxSpecific, AFTER_RX_SPECIFIC),
            (Stream::TxSpecific, AFTER_TX_SPECIFIC),
        ] {
            for _ in 0..INITIAL_BURST_REPEATS {
                src.send(stream, true)
                    .with_context(|| format!("send HPSDR protocol 2 {} packet", stream.name()))?;
            }
            thread::sleep(wait);
        }
        // MAN-259: the radio streams from run = 1 on; the bring-up before
        // it is still outage.
        src.acquisition_start_offset = entered.elapsed();
        for _ in 0..INITIAL_BURST_REPEATS {
            src.send(Stream::HighPriority, true)
                .context("send HPSDR protocol 2 start (high-priority run = 1)")?;
        }
        let now = Instant::now();
        src.last_high_priority = now;
        src.last_rx_tx_specific = now;
        src.last_general = now;
        src.last_valid_iq = now;
        src.last_accepted_iq = now;
        src.read_returned = now;
        Ok(src)
    }

    /// Loss and malformed counters observed so far (same shape as
    /// protocol 1's [`super::HpsdrIqSource::gap_stats`]).
    pub fn gap_stats(&self) -> GapStats {
        GapStats {
            dropped_packets: self.health_counters.dropped_packets(),
            gaps_detected: self.health_counters.gaps_detected(),
            malformed_packets: self.health_counters.malformed_packets(),
        }
    }

    /// Build and send the next packet of `stream`; `run` matters only for
    /// high-priority.
    fn send(&mut self, stream: Stream, run: bool) -> io::Result<()> {
        let i = stream as usize;
        let seq = self.seq[i];
        self.seq[i] = seq.wrapping_add(1);
        let packet: Vec<u8> = match stream {
            Stream::General => wire::general_packet(seq, &self.board).to_vec(),
            Stream::RxSpecific => wire::rx_specific_packet(seq, &self.board, self.rate_khz),
            Stream::TxSpecific => wire::tx_specific_packet(seq).to_vec(),
            Stream::HighPriority => wire::high_priority_packet(
                seq,
                run,
                self.phase_word,
                self.alex0,
                self.alex1,
                self.attenuation.as_ref().map_or(0, |a| a.controller.db()),
            ),
        };
        let to = SocketAddrV4::new(self.radio_ip, self.port + stream as u16);
        self.socket.send_to(&packet, to).map(|_| ())
    }

    /// D8: resend each command stream when its interval has passed.
    fn send_keepalive_if_due(&mut self) -> Result<()> {
        let now = Instant::now();
        if now.duration_since(self.last_high_priority) >= HIGH_PRIORITY_EVERY {
            self.last_high_priority = now;
            self.send(Stream::HighPriority, true)
                .context("send HPSDR protocol 2 high-priority keepalive")?;
        }
        if now.duration_since(self.last_rx_tx_specific) >= RX_TX_SPECIFIC_EVERY {
            self.last_rx_tx_specific = now;
            self.send(Stream::RxSpecific, true)
                .context("send HPSDR protocol 2 rx-specific keepalive")?;
            self.send(Stream::TxSpecific, true)
                .context("send HPSDR protocol 2 tx-specific keepalive")?;
        }
        if now.duration_since(self.last_general) >= GENERAL_EVERY {
            self.last_general = now;
            self.send(Stream::General, true)
                .context("send HPSDR protocol 2 general keepalive")?;
        }
        Ok(())
    }

    /// MAN-308: one high-priority status datagram. Without automatic
    /// attenuation it is ignored, as before; a datagram of another length is
    /// ignored too, not counted malformed.
    fn handle_status(&mut self, len: usize) -> Result<()> {
        let Some(att) = self.attenuation.as_mut() else {
            return Ok(());
        };
        let Some(bits) = wire::adc_overload_bits(&self.recv_buf[..len]) else {
            return Ok(());
        };
        match att
            .controller
            .observe(bits & wire::ADC0_OVERLOAD != 0, Instant::now())
        {
            Some(step) => self.apply_step(step),
            None => Ok(()),
        }
    }

    /// MAN-308: lower the attenuator once the band has been quiet long
    /// enough.
    fn tick_attenuation(&mut self) -> Result<()> {
        let Some(att) = self.attenuation.as_mut() else {
            return Ok(());
        };
        match att.controller.tick(Instant::now()) {
            Some(step) => self.apply_step(step),
            None => Ok(()),
        }
    }

    /// MAN-308: record and log one step, and send it to the radio now rather
    /// than at the next keepalive.
    fn apply_step(&mut self, step: AttenuationStep) -> Result<()> {
        if let Some(att) = &self.attenuation {
            att.handle.record(&AttenuationChange {
                radio: format!("{}:{}", self.host, self.port),
                from_db: step.from_db,
                to_db: step.to_db,
                cause: step.cause,
            });
        }
        self.last_high_priority = Instant::now();
        self.send(Stream::HighPriority, true)
            .context("send HPSDR protocol 2 high-priority attenuation change")
    }

    /// Count one malformed datagram and enforce the D13 flood bound.
    fn malformed(&mut self) -> Result<()> {
        self.health_counters.record_malformed();
        self.consecutive_malformed = self.consecutive_malformed.saturating_add(1);
        if self.consecutive_malformed >= self.max_consecutive_malformed {
            bail!(
                "HPSDR protocol 2 radio {} stalled: {} consecutive malformed datagrams with no \
                 valid IQ datagram -- corrupt hardware or a spoofed stream",
                self.host,
                self.max_consecutive_malformed
            );
        }
        Ok(())
    }

    /// Add `n` samples to a pending report.
    fn add_pending(slot: &mut Option<u64>, n: u64) {
        *slot = Some(slot.unwrap_or(0).saturating_add(n));
    }

    /// Handle one datagram from the IQ port: on success the queue holds its
    /// samples and any gap report is pending (D9/D10/D11).
    fn handle_iq(&mut self, len: usize) -> Result<()> {
        self.queue.clear();
        self.queue_pos = 0;
        let seq = match wire::parse_iq_datagram(&self.recv_buf[..len], &mut self.queue) {
            Ok(seq) => seq,
            Err(_) => return self.malformed(),
        };
        self.consecutive_malformed = 0;
        self.confirmed_live.store(true, Ordering::Relaxed);
        let now = Instant::now();
        let since_last = self.unaccounted(now);
        self.last_valid_iq = now;
        let event = self.tracker.observe(seq);
        if !matches!(event, wire::SeqEvent::Stale) {
            self.last_accepted_iq = now;
        }
        let reported = match event {
            wire::SeqEvent::InOrder => 0,
            wire::SeqEvent::Stale => {
                self.queue.clear();
                return Ok(());
            }
            // A jump no wall clock allows is a restart, not a loss: a
            // corrupt or spoofed sequence number must not move the spot
            // clock by days (the next real datagram then resyncs).
            wire::SeqEvent::Gap(k)
                if u64::from(k) > plausible_lost_datagrams(since_last, self.sample_rate_hz) =>
            {
                let outage = self.restart_outage(since_last);
                self.report_restart(outage)
            }
            wire::SeqEvent::Gap(k) => {
                self.health_counters.record_dropped(u64::from(k));
                self.health_counters.record_gap();
                let missing = u64::from(k) * wire::SAMPLES_PER_DATAGRAM as u64;
                if (missing as f64) < self.sample_rate_hz / 2.0 {
                    Self::add_pending(&mut self.pending_missing, missing);
                } else {
                    Self::add_pending(&mut self.pending_discontinuity, missing);
                }
                missing
            }
            wire::SeqEvent::Restart => {
                let outage = self.restart_outage(since_last);
                self.report_restart(outage)
            }
        };
        if let Some(p) = self.parked.as_mut() {
            p.accounted += wire::SAMPLES_PER_DATAGRAM as u64 + reported;
            p.left = p.left.saturating_sub(1);
            if p.left == 0 {
                self.parked = None;
            }
        }
        Ok(())
    }

    /// The wall clock a gap or restart is judged against (D10/D11): since
    /// the last accepted datagram, or, while a parked backlog drains
    /// (MAN-423), since the pause began less the samples accounted since,
    /// whichever is longer. A datagram drained from the backlog may have
    /// waited the whole pause, so the time since it arrived says nothing
    /// about how far the radio has moved on.
    fn unaccounted(&self, now: Instant) -> Duration {
        let since_last = now.duration_since(self.last_accepted_iq);
        match self.parked {
            Some(p) => {
                let accounted = Duration::from_secs_f64(p.accounted as f64 / self.sample_rate_hz);
                since_last.max(now.duration_since(p.from).saturating_sub(accounted))
            }
            None => since_last,
        }
    }

    /// MAN-423 review: the outage a restart reports. Outside a park it is
    /// `since_last`. While a parked backlog drains, the pause is only an
    /// upper bound: a radio that restarted early in the pause kept
    /// streaming, and what it sent since still waits on the socket (or in
    /// `look_ahead`), so the whole pause read as missing would run every
    /// later timestamp ahead by what that backlog holds (and count a later
    /// overflow gap twice). The backlog is received now, without blocking,
    /// and the radio time it represents -- every queued datagram, across any
    /// further restarts in it -- comes off the unaccounted pause. The
    /// datagrams are handled afterwards as if just received; a later
    /// restart among them measures the same way, so the pause is assigned
    /// exactly once.
    fn restart_outage(&mut self, since_last: Duration) -> Duration {
        if self.parked.is_none() {
            return since_last;
        }
        self.receive_backlog();
        let bound = self.unaccounted(Instant::now());
        let covered = Duration::from_secs_f64(
            self.backlog_datagrams(bound) as f64 * wire::SAMPLES_PER_DATAGRAM as f64
                / self.sample_rate_hz,
        );
        bound.saturating_sub(covered)
    }

    /// Receive what the socket already holds into `look_ahead`, up to
    /// [`MAX_BUFFERED_DATAGRAMS`] queued. A socket that cannot be made
    /// non-blocking adds nothing (the pause bound then stands).
    fn receive_backlog(&mut self) {
        if self.socket.set_nonblocking(true).is_err() {
            return;
        }
        let mut buf = vec![0u8; RECV_BUF_LEN];
        while self.look_ahead.len() < MAX_BUFFERED_DATAGRAMS as usize {
            let Ok((len, from)) = self.socket.recv_from(&mut buf) else {
                // WouldBlock: drained. Any other error surfaces on the
                // next blocking receive.
                break;
            };
            self.look_ahead.push_back((buf[..len].to_vec(), from));
        }
        // A failure here leaves the socket non-blocking; the next receive
        // reports it as an error rather than waiting.
        let _ = self.socket.set_nonblocking(false);
    }

    /// The datagrams of radio time `look_ahead`'s DDC0 IQ represents after
    /// the current one, judged as `handle_iq` will judge them (a copy of the
    /// tracker): each accepted datagram is one, a plausible gap also counts
    /// its lost datagrams, a stale one counts nothing. A jump `bound` cannot
    /// explain is a restart, as in `handle_iq`, and counts only itself.
    fn backlog_datagrams(&self, bound: Duration) -> u64 {
        let plausible = plausible_lost_datagrams(bound, self.sample_rate_hz);
        let mut tracker = self.tracker.clone();
        let mut scratch = Vec::with_capacity(wire::SAMPLES_PER_DATAGRAM);
        let mut total = 0u64;
        for (bytes, from) in &self.look_ahead {
            if !self.is_ddc0(*from) {
                continue;
            }
            scratch.clear();
            let Ok(seq) = wire::parse_iq_datagram(bytes, &mut scratch) else {
                continue;
            };
            total += match tracker.observe(seq) {
                wire::SeqEvent::InOrder | wire::SeqEvent::Restart => 1,
                wire::SeqEvent::Stale => 0,
                wire::SeqEvent::Gap(k) if u64::from(k) > plausible => 1,
                wire::SeqEvent::Gap(k) => u64::from(k) + 1,
            };
        }
        total
    }

    /// Whether `from` is the radio's DDC0 IQ port.
    fn is_ddc0(&self, from: SocketAddr) -> bool {
        let SocketAddr::V4(v4) = from else {
            return false;
        };
        *v4.ip() == self.radio_ip
            && from.port() >= self.port
            && u32::from(from.port()).wrapping_sub(u32::from(self.port))
                == u32::from(DDC0_PORT_OFFSET)
    }

    /// A restart (D10/D11): one gap, no dropped count (the true count is
    /// unknown), and a discontinuity estimated from the wall clock. Returns
    /// the samples it reported missing.
    fn report_restart(&mut self, since_last: Duration) -> u64 {
        self.health_counters.record_gap();
        let elapsed = (since_last.as_secs_f64() * self.sample_rate_hz).round();
        let missing = (elapsed - wire::SAMPLES_PER_DATAGRAM as f64).max(0.0) as u64;
        Self::add_pending(&mut self.pending_discontinuity, missing);
        missing
    }

    /// Receive datagrams until the queue holds DDC0 samples (D5 demux).
    fn fill_queue(&mut self) -> Result<()> {
        // D13's stall clock runs only while a read waits: time the caller
        // spent not reading (a multi-input run parks sources until every
        // opener finishes) is not the radio's silence, and the IQ it sent
        // meanwhile is still queued on the socket (MAN-305 review).
        let entered = Instant::now();
        while self.queue_pos >= self.queue.len() {
            if self.last_valid_iq.max(entered).elapsed() >= self.stall_after {
                bail!(
                    "HPSDR protocol 2 radio {host} stalled: no IQ datagram for {secs} s -- if the \
                     radio is on, allow inbound UDP from {host} in the host firewall (protocol 2 \
                     sends IQ from port {iq_port}, not a port manta sent to)",
                    host = self.host,
                    secs = self.stall_after.as_secs_f64(),
                    iq_port = u32::from(self.port) + u32::from(DDC0_PORT_OFFSET),
                );
            }
            self.send_keepalive_if_due()?;
            self.tick_attenuation()?;
            let (len, from) = if let Some((bytes, from)) = self.look_ahead.pop_front() {
                self.recv_buf[..bytes.len()].copy_from_slice(&bytes);
                (bytes.len(), from)
            } else {
                match self.socket.recv_from(&mut self.recv_buf) {
                    Ok(got) => got,
                    Err(e) => match classify_recv_error(&e) {
                        RecvErrorAction::Retry | RecvErrorAction::Timeout => continue,
                        RecvErrorAction::Fatal => {
                            return Err(e).context("receive from HPSDR protocol 2 radio");
                        }
                    },
                }
            };
            let from_radio = match from {
                SocketAddr::V4(v4) => *v4.ip() == self.radio_ip,
                SocketAddr::V6(_) => false,
            };
            if !from_radio {
                self.malformed()?;
                continue;
            }
            let offset = u32::from(from.port()).wrapping_sub(u32::from(self.port));
            if from.port() >= self.port && offset == u32::from(DDC0_PORT_OFFSET) {
                self.handle_iq(len)?;
            } else if from.port() >= self.port && offset == u32::from(STATUS_PORT_OFFSET) {
                self.handle_status(len)?;
            } else if from.port() >= self.port && offset <= u32::from(LAST_IGNORED_PORT_OFFSET) {
                // Command response, status, mic or wideband: not counted.
            } else {
                self.malformed()?;
            }
        }
        Ok(())
    }
}

/// The most datagrams that can really have been lost between two valid
/// datagrams `since_last` apart at `sample_rate_hz`: what the radio sent in
/// that time (with 1% for clock skew), plus one full socket buffer.
fn plausible_lost_datagrams(since_last: Duration, sample_rate_hz: f64) -> u64 {
    let per_second = sample_rate_hz / wire::SAMPLES_PER_DATAGRAM as f64;
    (since_last.as_secs_f64() * per_second * 1.01).ceil() as u64 + MAX_BUFFERED_DATAGRAMS
}

impl IqSource for Protocol2IqSource {
    fn sample_rate(&self) -> f64 {
        self.sample_rate_hz
    }

    fn center_freq_hz(&self) -> f64 {
        self.center_freq_hz
    }

    /// Returns samples of one datagram at most, so a read never straddles a
    /// gap: a gap is found only when the queue is empty and nothing has been
    /// copied in this call, so the report pending after a read always
    /// describes the samples that read returned (D11).
    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        // MAN-423: the caller paused; what the radio sent meanwhile is
        // queued on the socket, or dropped once the buffer filled. A pause
        // opens a window anchored where it began. A pause while an earlier
        // window is still open keeps that anchor and what it accounted --
        // `(now - from) - accounted` stays the radio time not yet seen, the
        // earlier pause's included -- and only refills `left`, since the
        // socket may now hold a full buffer again (PR review).
        if self.read_returned.elapsed() >= PARKED_AFTER {
            match self.parked.as_mut() {
                Some(p) => p.left = self.park_window,
                None => {
                    self.parked = Some(Parked {
                        from: self.read_returned,
                        accounted: 0,
                        left: self.park_window,
                    });
                }
            }
        }
        self.fill_queue()?;
        let avail = &self.queue[self.queue_pos..];
        let n = buf.len().min(avail.len());
        buf[..n].copy_from_slice(&avail[..n]);
        self.queue_pos += n;
        self.read_returned = Instant::now();
        Ok(n)
    }

    fn take_discontinuity(&mut self) -> Option<u64> {
        self.pending_discontinuity.take()
    }

    fn take_missing_samples(&mut self) -> Option<u64> {
        self.pending_missing.take()
    }

    fn confirmed_live_handle(&self) -> Option<Arc<AtomicBool>> {
        Some(self.confirmed_live.clone())
    }

    fn health_counters(&self) -> Option<Arc<InputHealthCounters>> {
        Some(self.health_counters.clone())
    }

    fn acquisition_start_offset(&self) -> Duration {
        self.acquisition_start_offset
    }
}

impl Drop for Protocol2IqSource {
    /// D8: tell the radio to stop streaming; errors are ignored.
    fn drop(&mut self) {
        for _ in 0..STOP_REPEATS {
            let _ = self.send(Stream::HighPriority, false);
        }
    }
}

/// A fake protocol-2 radio on loopback for this module's and `hpsdr.rs`'s
/// tests (MAN-305 Phase 3/4).
#[cfg(test)]
pub(crate) mod fake {
    use super::wire;
    use crate::hpsdr::HpsdrConfig;
    use std::net::{SocketAddr, UdpSocket};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    /// One datagram the fake received: port offset from `base`, bytes,
    /// source.
    pub(crate) type Record = (u16, Vec<u8>, SocketAddr);

    /// Binds `base..=base+3` and `base+11` on 127.0.0.1 (any free base),
    /// records what each port receives, and answers discovery on `base`
    /// with the scripted replies.
    pub(crate) struct FakeP2Radio {
        pub(crate) base: u16,
        cmd: Vec<UdpSocket>,
        iq: UdpSocket,
        log: Arc<Mutex<Vec<Record>>>,
        stop: Arc<AtomicBool>,
        threads: Vec<JoinHandle<()>>,
        /// Records before this index are ignored by `wait_for_run`, so a
        /// reopen streams to the new socket (MAN-308).
        run_floor: Mutex<usize>,
    }

    /// Stops a background sender when dropped.
    pub(crate) struct Background {
        stop: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }

    impl Drop for Background {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    fn is_discovery_request(d: &[u8]) -> bool {
        (d.len() == 60 && d[4] == 0x02 && d.iter().enumerate().all(|(i, &b)| i == 4 || b == 0))
            || (d.len() == 63 && d[..3] == [0xEF, 0xFE, 0x02])
    }

    /// A 1444-byte DDC IQ datagram: sample k is I = seq mod 2^22, Q = -k.
    pub(crate) fn iq_datagram(seq: u32) -> Vec<u8> {
        let mut p = vec![0u8; wire::IQ_DATAGRAM_LEN];
        p[0..4].copy_from_slice(&seq.to_be_bytes());
        p[12..14].copy_from_slice(&wire::BITS_PER_SAMPLE.to_be_bytes());
        p[14..16].copy_from_slice(&(wire::SAMPLES_PER_DATAGRAM as u16).to_be_bytes());
        let i = (seq % (1 << 22)) as i32;
        for k in 0..wire::SAMPLES_PER_DATAGRAM {
            let at = 16 + 6 * k;
            p[at..at + 3].copy_from_slice(&i.to_be_bytes()[1..4]);
            p[at + 3..at + 6].copy_from_slice(&(-(k as i32)).to_be_bytes()[1..4]);
        }
        p
    }

    impl FakeP2Radio {
        pub(crate) fn start() -> Self {
            Self::with_discovery(Vec::new())
        }

        /// `answers` (delay, reply) are sent in order to the first
        /// discovery request seen on `base`.
        pub(crate) fn with_discovery(answers: Vec<(Duration, Vec<u8>)>) -> Self {
            let (base, cmd, iq) = Self::bind();
            let log = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let mut threads = Vec::new();
            let answers = Arc::new(Mutex::new(Some(answers)));
            for (offset, sock) in (0u16..).zip(cmd.iter()) {
                let sock = sock.try_clone().unwrap();
                sock.set_read_timeout(Some(Duration::from_millis(10)))
                    .unwrap();
                let (log, stop, answers) = (log.clone(), stop.clone(), answers.clone());
                threads.push(thread::spawn(move || {
                    let mut buf = vec![0u8; 65536];
                    while !stop.load(Ordering::SeqCst) {
                        let Ok((n, from)) = sock.recv_from(&mut buf) else {
                            continue;
                        };
                        let d = buf[..n].to_vec();
                        let discovery = offset == 0 && is_discovery_request(&d);
                        log.lock().unwrap().push((offset, d, from));
                        if discovery {
                            if let Some(list) = answers.lock().unwrap().take() {
                                for (delay, reply) in list {
                                    thread::sleep(delay);
                                    let _ = sock.send_to(&reply, from);
                                }
                            }
                        }
                    }
                }));
            }
            Self {
                base,
                cmd,
                iq,
                log,
                stop,
                threads,
                run_floor: Mutex::new(0),
            }
        }

        fn bind() -> (u16, Vec<UdpSocket>, UdpSocket) {
            for _ in 0..200 {
                let first = UdpSocket::bind("127.0.0.1:0").unwrap();
                let base = first.local_addr().unwrap().port();
                if base > u16::MAX - 11 {
                    continue;
                }
                let mut cmd = vec![first];
                let rest: Option<Vec<UdpSocket>> = (1..=3)
                    .map(|o| UdpSocket::bind(("127.0.0.1", base + o)).ok())
                    .collect();
                let Some(rest) = rest else { continue };
                let Ok(iq) = UdpSocket::bind(("127.0.0.1", base + 11)) else {
                    continue;
                };
                cmd.extend(rest);
                return (base, cmd, iq);
            }
            panic!("no free base port for FakeP2Radio");
        }

        /// A one-DDC, 192 kHz, 7.03 MHz config pointing at this fake.
        pub(crate) fn cfg(&self) -> HpsdrConfig {
            HpsdrConfig {
                host: "127.0.0.1".to_string(),
                port: self.base,
                ddc_count: 1,
                sample_rate_hz: 192_000.0,
                center_freq_hz: vec![7_030_000.0],
                auto_attenuation: None,
            }
        }

        pub(crate) fn records(&self) -> Vec<Record> {
            self.log.lock().unwrap().clone()
        }

        /// Every datagram received on `base + offset`, in order.
        pub(crate) fn received(&self, offset: u16) -> Vec<Vec<u8>> {
            self.records()
                .into_iter()
                .filter(|r| r.0 == offset)
                .map(|r| r.1)
                .collect()
        }

        /// Wait (up to 3 s) for high-priority with run = 1 and return the
        /// address it came from: manta's streaming socket.
        pub(crate) fn wait_for_run(&self) -> SocketAddr {
            let deadline = Instant::now() + Duration::from_secs(3);
            let floor = *self.run_floor.lock().unwrap();
            loop {
                if let Some(r) =
                    self.records().into_iter().skip(floor).find(|r| {
                        r.0 == 3 && r.1.len() == wire::HIGH_PRIORITY_LEN && r.1[4] & 1 == 1
                    })
                {
                    return r.2;
                }
                assert!(Instant::now() < deadline, "no run = 1 from manta");
                thread::sleep(Duration::from_millis(5));
            }
        }

        /// Make `wait_for_run` (and every sender) wait for a run = 1 sent
        /// after now: the next open's, once the last source has dropped.
        pub(crate) fn forget_runs(&self) {
            *self.run_floor.lock().unwrap() = self.records().len();
        }

        /// Send `bytes` from the IQ port, only once run = 1 was seen.
        pub(crate) fn send_iq_bytes(&self, bytes: &[u8]) {
            let to = self.wait_for_run();
            self.iq.send_to(bytes, to).unwrap();
        }

        pub(crate) fn send_iq(&self, seq: u32) {
            self.send_iq_bytes(&iq_datagram(seq));
        }

        /// Send `bytes` from `base + offset` (0..=3) to manta.
        pub(crate) fn send_from(&self, offset: u16, bytes: &[u8]) {
            let to = self.wait_for_run();
            self.cmd[usize::from(offset)].send_to(bytes, to).unwrap();
        }

        /// Send in-order IQ datagrams from seq 0 every `every` until dropped.
        pub(crate) fn stream_iq(&self, every: Duration) -> Background {
            let to = self.wait_for_run();
            let sock = self.iq.try_clone().unwrap();
            self.background(every, move |n| {
                let _ = sock.send_to(&iq_datagram(n), to);
            })
        }

        /// Send a status datagram from `base + 1` every `every` until dropped.
        pub(crate) fn stream_status(&self, every: Duration) -> Background {
            self.stream_status_bytes(every, [0u8; 60])
        }

        /// Send `status` from `base + 1` every `every` until dropped.
        pub(crate) fn stream_status_bytes(&self, every: Duration, status: [u8; 60]) -> Background {
            let to = self.wait_for_run();
            let sock = self.cmd[1].try_clone().unwrap();
            self.background(every, move |_| {
                let _ = sock.send_to(&status, to);
            })
        }

        /// Send one status datagram (any length) from `base + 1`.
        pub(crate) fn send_status(&self, status: &[u8]) {
            self.send_from(1, status);
        }

        fn background<F: FnMut(u32) + Send + 'static>(
            &self,
            every: Duration,
            mut f: F,
        ) -> Background {
            let stop = Arc::new(AtomicBool::new(false));
            let s = stop.clone();
            let handle = thread::spawn(move || {
                let mut n = 0u32;
                while !s.load(Ordering::SeqCst) {
                    f(n);
                    n = n.wrapping_add(1);
                    thread::sleep(every);
                }
            });
            Background {
                stop,
                handle: Some(handle),
            }
        }
    }

    impl Drop for FakeP2Radio {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            for t in self.threads.drain(..) {
                let _ = t.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{iq_datagram, FakeP2Radio};
    use super::*;
    use std::net::SocketAddr;
    use std::sync::Mutex;

    /// A read buffer larger than any datagram, so a read that returned more
    /// than one datagram's samples would show.
    const BIG: usize = 4096;

    fn open(fake: &FakeP2Radio) -> Protocol2IqSource {
        Protocol2IqSource::open(&fake.cfg(), wire::board_info(Some(4))).unwrap()
    }

    /// The datagram a read's samples came from (fake: I = seq).
    fn seq_of(buf: &[Complex32]) -> u32 {
        (buf[0].re * wire::P2_IQ_FULL_SCALE).round() as u32
    }

    /// Read one datagram's worth with a big buffer: 238 samples, all from
    /// one datagram.
    fn read_one(src: &mut Protocol2IqSource) -> (u32, usize) {
        let mut buf = vec![Complex32::new(0.0, 0.0); BIG];
        let n = src.read(&mut buf).unwrap();
        let seq = seq_of(&buf[..n]);
        assert!(
            buf[..n]
                .iter()
                .all(|s| (s.re * wire::P2_IQ_FULL_SCALE).round() as u32 == seq),
            "a read straddled two datagrams"
        );
        (seq, n)
    }

    #[test]
    fn open_sends_general_rx_tx_then_run_in_that_order() {
        let fake = FakeP2Radio::start();
        let src = open(&fake);
        fake.wait_for_run();
        thread::sleep(Duration::from_millis(20));
        let board = wire::board_info(Some(4));
        let phase = wire::phase_word(7_030_000.0);
        let (a0, a1) = wire::alex_words(board.scheme, 7_030_000.0);
        let first: Vec<(u16, Vec<u8>)> = fake
            .records()
            .into_iter()
            .take(8)
            .map(|r| (r.0, r.1))
            .collect();
        let expected = vec![
            (0, wire::general_packet(0, &board).to_vec()),
            (0, wire::general_packet(1, &board).to_vec()),
            (1, wire::rx_specific_packet(0, &board, 192)),
            (1, wire::rx_specific_packet(1, &board, 192)),
            (2, wire::tx_specific_packet(0).to_vec()),
            (2, wire::tx_specific_packet(1).to_vec()),
            (3, wire::high_priority_packet(0, true, phase, a0, a1, 0)),
            (3, wire::high_priority_packet(1, true, phase, a0, a1, 0)),
        ];
        assert_eq!(first, expected);
        drop(src);
    }

    #[test]
    fn streams_iq_from_ddc0() {
        let fake = FakeP2Radio::start();
        let mut src = open(&fake);
        let mut expected = Vec::new();
        for seq in 0..4 {
            fake.send_iq(seq);
            wire::parse_iq_datagram(&iq_datagram(seq), &mut expected).unwrap();
        }
        let mut got = Vec::new();
        let mut buf = vec![Complex32::new(0.0, 0.0); 100];
        while got.len() < 952 {
            let n = src.read(&mut buf).unwrap();
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got.len(), 952);
        assert_eq!(got, expected);
        assert_eq!(src.gap_stats(), GapStats::default());
    }

    #[test]
    fn iq_before_run_is_never_requested() {
        let fake = FakeP2Radio::start();
        let mut src = open(&fake);
        // Everything before the first run = 1 configures the radio; the
        // fake streams only after it. Wait until the fake has logged it.
        fake.wait_for_run();
        let records = fake.records();
        let run_at = records
            .iter()
            .position(|r| r.0 == 3 && r.1[4] & 1 == 1)
            .expect("run = 1 sent");
        assert!(records[..run_at].iter().all(|r| r.0 <= 2));
        assert_eq!(run_at, 6);
        fake.send_iq(0);
        assert_eq!(read_one(&mut src), (0, 238));
    }

    /// Send `seqs` in batches of 20, reading each batch back with a big
    /// buffer; returns (seq, take_missing, take_discontinuity) per read.
    fn feed(
        fake: &FakeP2Radio,
        src: &mut Protocol2IqSource,
        seqs: &[u32],
    ) -> Vec<(u32, Option<u64>, Option<u64>)> {
        let mut out = Vec::new();
        for batch in seqs.chunks(20) {
            for &s in batch {
                fake.send_iq(s);
            }
            for _ in batch {
                let (seq, n) = read_one(src);
                assert_eq!(n, 238);
                out.push((seq, src.take_missing_samples(), src.take_discontinuity()));
            }
        }
        out
    }

    #[test]
    fn fifty_lost_datagrams_count_fifty_and_report_11900_missing_samples() {
        let fake = FakeP2Radio::start();
        let mut src = open(&fake);
        let seqs: Vec<u32> = (0..100).chain(150..200).collect();
        let reads = feed(&fake, &mut src, &seqs);
        assert_eq!(reads.iter().map(|r| r.0).collect::<Vec<_>>(), seqs);
        let reports: Vec<_> = reads.iter().filter(|r| r.1.is_some()).collect();
        assert_eq!(reports, vec![&(150, Some(11_900), None)]);
        assert!(reads.iter().all(|r| r.2.is_none()));
        let stats = src.gap_stats();
        assert_eq!(stats.dropped_packets, 50);
        assert_eq!(stats.gaps_detected, 1);
        assert_eq!(stats.malformed_packets, 0);
    }

    #[test]
    fn a_gap_of_403_datagrams_is_spliced() {
        let fake = FakeP2Radio::start();
        let mut src = open(&fake);
        let reads = feed(&fake, &mut src, &[0, 404]);
        assert_eq!(reads, vec![(0, None, None), (404, Some(95_914), None)]);
        assert_eq!(src.gap_stats().dropped_packets, 403);
    }

    #[test]
    fn a_gap_of_404_datagrams_is_a_discontinuity() {
        let fake = FakeP2Radio::start();
        let mut src = open(&fake);
        let reads = feed(&fake, &mut src, &[0, 405]);
        assert_eq!(reads, vec![(0, None, None), (405, None, Some(96_152))]);
        assert_eq!(src.gap_stats().dropped_packets, 404);
    }

    /// The restart estimate is wall clock from the last accepted datagram to
    /// the restart one, so bound it by what the test itself measured around
    /// both reads: a fixed window flakes on a loaded runner whose 300 ms
    /// sleep overshoots by 50-150 ms (macOS CI, MAN-305).
    fn assert_wall_clock_discontinuity(d: u64, at_least: Duration, at_most: Duration) {
        let samples = |t: Duration| t.as_secs_f64() * 192_000.0;
        let lo = (samples(at_least) - wire::SAMPLES_PER_DATAGRAM as f64 - 1.0).max(0.0);
        let hi = samples(at_most) + 1.0;
        assert!(
            (lo..=hi).contains(&(d as f64)),
            "discontinuity {d}, measured {at_least:?}..={at_most:?} => {lo:.0}..={hi:.0}"
        );
    }

    #[test]
    fn radio_restart_resyncs_with_a_wall_clock_discontinuity() {
        let fake = FakeP2Radio::start();
        let mut src = open(&fake);
        let seqs: Vec<u32> = (0..10).collect();
        let before_last = Instant::now();
        feed(&fake, &mut src, &seqs);
        let after_last = Instant::now();
        thread::sleep(Duration::from_millis(300));
        let before_restart = Instant::now();
        let reads = feed(&fake, &mut src, &seqs);
        let after_restart = Instant::now();
        assert_eq!(reads[0].0, 0);
        assert_eq!(reads[0].1, None);
        let d = reads[0].2.expect("restart reports a discontinuity");
        // The 300 ms pause parks the socket (MAN-423), so the nine
        // post-restart datagrams already queued behind sequence 0 count as
        // radio time since the restart, not as outage (MAN-423 review).
        let queued = Duration::from_secs_f64(9.0 * 238.0 / 192_000.0);
        assert_wall_clock_discontinuity(
            d,
            (before_restart - after_last).saturating_sub(queued),
            (after_restart - before_last).saturating_sub(queued),
        );
        assert!(reads[1..].iter().all(|r| r.1.is_none() && r.2.is_none()));
        let stats = src.gap_stats();
        assert_eq!(stats.dropped_packets, 0);
        assert_eq!(stats.gaps_detected, 1);
    }

    #[test]
    fn a_restart_after_stale_datagrams_measures_the_whole_outage() {
        let fake = FakeP2Radio::start();
        let mut src = open(&fake);
        let seqs: Vec<u32> = (0..200).collect();
        let before_last = Instant::now();
        feed(&fake, &mut src, &seqs);
        let after_last = Instant::now();
        thread::sleep(Duration::from_millis(300));
        let before_restart = Instant::now();
        // Seven stale datagrams, then the eighth restarts the stream: the
        // estimate still runs from 199, not from the last stale one.
        for s in 190..197 {
            fake.send_iq(s);
        }
        fake.send_iq(197);
        assert_eq!(read_one(&mut src), (197, 238));
        let after_restart = Instant::now();
        let d = src
            .take_discontinuity()
            .expect("restart reports a discontinuity");
        assert_wall_clock_discontinuity(
            d,
            before_restart - after_last,
            after_restart - before_last,
        );
        assert_eq!(src.gap_stats().dropped_packets, 0);
    }

    #[test]
    fn acquisition_starts_after_the_bring_up_before_run() {
        let fake = FakeP2Radio::start();
        let start = Instant::now();
        let src = open(&fake);
        let open_took = start.elapsed();
        let offset = src.acquisition_start_offset();
        let bring_up = AFTER_GENERAL + AFTER_RX_SPECIFIC + AFTER_TX_SPECIFIC;
        assert!(
            offset >= bring_up,
            "offset {offset:?} < bring-up {bring_up:?}"
        );
        assert!(
            offset <= open_took,
            "offset {offset:?} > open {open_took:?}"
        );
    }

    #[test]
    fn an_impossible_forward_jump_is_a_restart_not_a_loss() {
        let fake = FakeP2Radio::start();
        let mut src = open(&fake);
        // 2^30 datagrams is ~15 days at 192 kHz; the next real datagram
        // (11) is then far behind and resyncs as a restart too.
        // (The fake's I is seq mod 2^22, so the jump reads back as 3.)
        let reads = feed(&fake, &mut src, &[0, 1, 2, (1 << 30) + 3, 11]);
        assert_eq!(
            reads.iter().map(|r| r.0).collect::<Vec<_>>(),
            [0, 1, 2, 3, 11]
        );
        assert!(reads.iter().all(|r| r.1.is_none()), "{reads:?}");
        let clock: u64 = reads.iter().filter_map(|r| r.2).sum();
        assert!(clock < 192_000, "the clock moved {clock} samples");
        let stats = src.gap_stats();
        assert_eq!(stats.dropped_packets, 0);
        assert_eq!(stats.gaps_detected, 2);
    }

    #[test]
    fn plausible_loss_allows_a_full_socket_buffer_beyond_the_wall_clock() {
        assert_eq!(plausible_lost_datagrams(Duration::ZERO, 192_000.0), 8192);
        // One second at 192 kHz is 806.7 datagrams; 1% skew on top.
        assert_eq!(
            plausible_lost_datagrams(Duration::from_secs(1), 192_000.0),
            815 + 8192
        );
    }

    #[test]
    fn wrap_is_not_a_gap() {
        let fake = FakeP2Radio::start();
        let mut src = open(&fake);
        for seq in [0xFFFF_FFFE, 0xFFFF_FFFF, 0, 1] {
            fake.send_iq(seq);
        }
        for _ in 0..4 {
            read_one(&mut src);
            assert_eq!(src.take_missing_samples(), None);
            assert_eq!(src.take_discontinuity(), None);
        }
        assert_eq!(src.gap_stats(), GapStats::default());
    }

    #[test]
    fn confirmed_live_flips_on_first_valid_iq_datagram_only() {
        let fake = FakeP2Radio::start();
        let mut src = open(&fake);
        let live = src.confirmed_live_handle().unwrap();
        fake.send_from(1, &[0u8; 60]);
        fake.send_from(0, &[0u8; 60]);
        src.stall_after = Duration::from_millis(400);
        let mut buf = vec![Complex32::new(0.0, 0.0); BIG];
        assert!(src.read(&mut buf).is_err());
        assert!(!live.load(Ordering::SeqCst));
        src.stall_after = STALL_AFTER;
        fake.send_iq(0);
        read_one(&mut src);
        assert!(live.load(Ordering::SeqCst));
        assert_eq!(src.gap_stats().malformed_packets, 0);
    }

    #[test]
    fn malformed_and_foreign_datagrams_are_counted_not_fatal() {
        let fake = FakeP2Radio::start();
        let mut src = open(&fake);
        let to = fake.wait_for_run();
        fake.send_iq_bytes(&[0u8; 100]);
        let foreign = UdpSocket::bind(("127.0.0.1", fake.base + 40))
            .or_else(|_| UdpSocket::bind("127.0.0.1:0"))
            .unwrap();
        let fport = foreign.local_addr().unwrap().port();
        assert!(!(fake.base..=fake.base + 11).contains(&fport));
        foreign.send_to(&iq_datagram(7), to).unwrap();
        fake.send_from(1, &[0u8; 60]);
        fake.send_iq(0);
        assert_eq!(read_one(&mut src), (0, 238));
        assert_eq!(src.gap_stats().malformed_packets, 2);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn datagrams_from_another_address_are_malformed() {
        let fake = FakeP2Radio::start();
        let mut src = open(&fake);
        let to = fake.wait_for_run();
        let other = UdpSocket::bind("127.0.0.2:0").unwrap();
        other.send_to(&iq_datagram(5), to).unwrap();
        fake.send_iq(0);
        assert_eq!(read_one(&mut src), (0, 238));
        assert_eq!(src.gap_stats().malformed_packets, 1);
    }

    #[test]
    fn stall_without_iq_errors_naming_the_firewall() {
        assert_eq!(STALL_AFTER, Duration::from_secs(10));
        let fake = FakeP2Radio::start();
        let mut src = open(&fake);
        src.stall_after = Duration::from_millis(300);
        let start = Instant::now();
        let mut buf = vec![Complex32::new(0.0, 0.0); BIG];
        let err = src.read(&mut buf).unwrap_err().to_string();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_eq!(
            err,
            format!(
                "HPSDR protocol 2 radio 127.0.0.1 stalled: no IQ datagram for 0.3 s -- if the \
                 radio is on, allow inbound UDP from 127.0.0.1 in the host firewall (protocol 2 \
                 sends IQ from port {}, not a port manta sent to)",
                fake.base + 11
            )
        );
    }

    #[test]
    fn a_pause_between_reads_is_not_a_stall() {
        let fake = FakeP2Radio::start();
        let mut src = open(&fake);
        src.stall_after = Duration::from_millis(300);
        fake.send_iq(0);
        assert_eq!(read_one(&mut src), (0, 238));
        // The caller stops reading for longer than the stall bound while
        // the radio keeps sending: the queued IQ is read, not a stall.
        fake.send_iq(1);
        thread::sleep(Duration::from_millis(600));
        assert_eq!(read_one(&mut src), (1, 238));
    }

    #[test]
    fn status_datagrams_do_not_hide_a_stall() {
        let fake = FakeP2Radio::start();
        let mut src = open(&fake);
        src.stall_after = Duration::from_millis(500);
        let _status = fake.stream_status(Duration::from_millis(20));
        let start = Instant::now();
        let mut buf = vec![Complex32::new(0.0, 0.0); BIG];
        let err = src.read(&mut buf).unwrap_err().to_string();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(err.contains("stalled: no IQ datagram"), "{err}");
    }

    /// MAN-308: a handle whose notice records every change.
    fn recording_handle() -> (Arc<AutoAttenuation>, Arc<Mutex<Vec<AttenuationChange>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        let handle = Arc::new(AutoAttenuation::new(Box::new(move |c| {
            s.lock().unwrap().push(c.clone())
        })));
        (handle, seen)
    }

    /// MAN-308: open with `handle` and test timings (trip after `trip`,
    /// release after `release`).
    fn open_attenuating(
        fake: &FakeP2Radio,
        handle: &Arc<AutoAttenuation>,
        trip: Duration,
        release: Duration,
    ) -> Protocol2IqSource {
        let cfg = HpsdrConfig {
            auto_attenuation: Some(handle.clone()),
            ..fake.cfg()
        };
        let mut src = Protocol2IqSource::open(&cfg, wire::board_info(Some(4))).unwrap();
        src.attenuation.as_mut().unwrap().controller.timing = Timing {
            hold: Duration::from_millis(100),
            trip_after: trip,
            release_after: release,
        };
        src
    }

    /// Read until `done` holds (3 s deadline), then `extra` more.
    fn read_until(src: &mut Protocol2IqSource, done: impl Fn() -> bool, extra: Duration) {
        let mut buf = vec![Complex32::new(0.0, 0.0); BIG];
        let deadline = Instant::now() + Duration::from_secs(3);
        while !done() {
            assert!(Instant::now() < deadline, "condition not reached in 3 s");
            src.read(&mut buf).unwrap();
        }
        let until = Instant::now() + extra;
        while Instant::now() < until {
            src.read(&mut buf).unwrap();
        }
    }

    /// A status datagram with `bits` in byte 5.
    fn status(bits: u8) -> [u8; 60] {
        let mut d = [0u8; 60];
        d[5] = bits;
        d
    }

    /// Byte 1443 (ADC0's step attenuator) of every high-priority packet
    /// with run = 1, in send order; asserts byte 1442 stays 0.
    fn adc0_attenuation_sent(fake: &FakeP2Radio) -> Vec<u8> {
        let hp: Vec<Vec<u8>> = fake
            .received(3)
            .into_iter()
            .filter(|p| p[4] & 1 == 1)
            .collect();
        assert!(hp.iter().all(|p| p[1442] == 0), "ADC1 attenuator moved");
        hp.iter().map(|p| p[1443]).collect()
    }

    fn high_priority_seqs(fake: &FakeP2Radio) -> Vec<u32> {
        fake.received(3)
            .iter()
            .map(|p| u32::from_be_bytes([p[0], p[1], p[2], p[3]]))
            .collect()
    }

    fn radio(fake: &FakeP2Radio) -> String {
        format!("127.0.0.1:{}", fake.base)
    }

    #[test]
    fn overload_raises_adc0_attenuation_on_the_wire() {
        let fake = FakeP2Radio::start();
        let (handle, seen) = recording_handle();
        let mut src = open_attenuating(&fake, &handle, Duration::ZERO, Duration::from_secs(600));
        let _iq = fake.stream_iq(Duration::from_millis(2));
        // The pure controller tests pin the hold and trip boundaries. Here
        // one report trips immediately, so scheduler delays cannot break a
        // shortened overload run or add an unintended second step.
        fake.send_status(&status(0x01));
        let h = handle.clone();
        read_until(&mut src, move || h.db() == 6, Duration::from_millis(200));
        drop(_iq);
        thread::sleep(Duration::from_millis(20));
        let sent = adc0_attenuation_sent(&fake);
        let first_six = sent.iter().position(|&b| b == 6).expect("6 dB sent");
        assert!(first_six > 0, "{sent:?}");
        assert!(sent[..first_six].iter().all(|&b| b == 0), "{sent:?}");
        assert!(sent[first_six..].iter().all(|&b| b == 6), "{sent:?}");
        assert_eq!(
            *seen.lock().unwrap(),
            vec![AttenuationChange {
                radio: radio(&fake),
                from_db: 0,
                to_db: 6,
                cause: attenuator::AttenuationCause::Overload,
            }]
        );
        assert_eq!(handle.changes(), 1);
        let seqs = high_priority_seqs(&fake);
        assert_eq!(seqs, (0..seqs.len() as u32).collect::<Vec<_>>());
    }

    #[test]
    fn reopen_restores_the_learned_level_and_quiet_lowers_it() {
        let fake = FakeP2Radio::start();
        let (handle, seen) = recording_handle();
        let mut src = open_attenuating(&fake, &handle, Duration::ZERO, Duration::from_secs(600));
        {
            let _iq = fake.stream_iq(Duration::from_millis(2));
            // Drive each step explicitly, as in the on-wire raise test.
            for db in [6, 12] {
                fake.send_status(&status(0x01));
                let h = handle.clone();
                read_until(&mut src, move || h.db() == db, Duration::ZERO);
            }
        }
        drop(src);
        thread::sleep(Duration::from_millis(50));
        let before_reopen = fake.received(3).len();
        fake.forget_runs();

        let mut src = open_attenuating(
            &fake,
            &handle,
            Duration::from_millis(200),
            Duration::from_millis(400),
        );
        fake.wait_for_run();
        let deadline = Instant::now() + Duration::from_secs(3);
        let bring_up: Vec<Vec<u8>> = loop {
            let runs: Vec<Vec<u8>> = fake.received(3)[before_reopen..]
                .iter()
                .filter(|p| p[4] & 1 == 1)
                .take(2)
                .cloned()
                .collect();
            if runs.len() == 2 {
                break runs;
            }
            assert!(Instant::now() < deadline, "no bring-up after the reopen");
            thread::sleep(Duration::from_millis(5));
        };
        assert!(
            bring_up.iter().all(|p| p[1443] == 12),
            "the reopen restores 12 dB"
        );
        let _iq = fake.stream_iq(Duration::from_millis(2));
        let _status = fake.stream_status_bytes(Duration::from_millis(20), status(0x00));
        let h = handle.clone();
        read_until(&mut src, move || h.db() == 6, Duration::from_millis(100));
        drop(_status);
        drop(_iq);
        thread::sleep(Duration::from_millis(20));
        let after: Vec<u8> = fake.received(3)[before_reopen..]
            .iter()
            .filter(|p| p[4] & 1 == 1)
            .map(|p| p[1443])
            .collect();
        let first_six = after.iter().position(|&b| b == 6).expect("6 dB sent");
        assert!(after[..first_six].iter().all(|&b| b == 12), "{after:?}");
        assert!(after[first_six..].iter().all(|&b| b == 6), "{after:?}");
        let changes = seen.lock().unwrap().clone();
        assert_eq!(changes.len(), 3);
        assert_eq!(
            changes[2],
            AttenuationChange {
                radio: radio(&fake),
                from_db: 12,
                to_db: 6,
                cause: attenuator::AttenuationCause::Quiet,
            }
        );
        assert_eq!(handle.changes(), 3);
        drop(src);
    }

    #[test]
    fn overload_changes_nothing_without_auto_attenuation() {
        let fake = FakeP2Radio::start();
        let mut src = open(&fake);
        let _iq = fake.stream_iq(Duration::from_millis(2));
        let _status = fake.stream_status_bytes(Duration::from_millis(20), status(0x01));
        let start = Instant::now();
        let mut buf = vec![Complex32::new(0.0, 0.0); BIG];
        while start.elapsed() < Duration::from_millis(500) {
            src.read(&mut buf).unwrap();
        }
        drop(_status);
        drop(_iq);
        thread::sleep(Duration::from_millis(20));
        let hp = fake.received(3);
        assert!(hp.iter().all(|p| p[1442] == 0 && p[1443] == 0));
        assert_eq!(src.gap_stats(), GapStats::default());
    }

    #[test]
    fn a_status_datagram_of_another_length_is_ignored() {
        let fake = FakeP2Radio::start();
        let (handle, seen) = recording_handle();
        let mut src = open_attenuating(
            &fake,
            &handle,
            Duration::from_millis(200),
            Duration::from_secs(600),
        );
        let _iq = fake.stream_iq(Duration::from_millis(2));
        let mut short = [0u8; 59];
        short[5] = 0x01;
        let start = Instant::now();
        let mut buf = vec![Complex32::new(0.0, 0.0); BIG];
        let mut last_status = Instant::now() - Duration::from_secs(1);
        while start.elapsed() < Duration::from_millis(500) {
            if last_status.elapsed() >= Duration::from_millis(20) {
                fake.send_status(&short);
                last_status = Instant::now();
            }
            src.read(&mut buf).unwrap();
        }
        assert_eq!(handle.db(), 0);
        assert_eq!(handle.changes(), 0);
        assert!(seen.lock().unwrap().is_empty());
        assert_eq!(src.gap_stats().malformed_packets, 0);
    }

    #[test]
    fn malformed_flood_errors_after_the_bound() {
        assert_eq!(MAX_CONSECUTIVE_MALFORMED, 10_000);
        let fake = FakeP2Radio::start();
        let mut src = open(&fake);
        src.max_consecutive_malformed = 50;
        for _ in 0..60 {
            fake.send_iq_bytes(&[0u8; 1443]);
        }
        let mut buf = vec![Complex32::new(0.0, 0.0); BIG];
        let err = src.read(&mut buf).unwrap_err().to_string();
        assert_eq!(
            err,
            "HPSDR protocol 2 radio 127.0.0.1 stalled: 50 consecutive malformed datagrams with no \
             valid IQ datagram -- corrupt hardware or a spoofed stream"
        );
        assert_eq!(src.gap_stats().malformed_packets, 50);
    }

    #[test]
    fn keepalive_resends_on_cadence() {
        let fake = FakeP2Radio::start();
        let mut src = open(&fake);
        let _iq = fake.stream_iq(Duration::from_millis(2));
        let before: Vec<usize> = (0..4).map(|o| fake.received(o).len()).collect();
        let start = Instant::now();
        let mut buf = vec![Complex32::new(0.0, 0.0); BIG];
        while start.elapsed() < Duration::from_secs(1) {
            src.read(&mut buf).unwrap();
        }
        thread::sleep(Duration::from_millis(20));
        let sent: Vec<usize> = (0..4)
            .map(|o| fake.received(o).len() - before[o as usize])
            .collect();
        assert!(sent[3] >= 8, "high-priority {sent:?}");
        assert!(sent[1] >= 4 && sent[2] >= 4, "rx/tx-specific {sent:?}");
        assert!(sent[0] >= 1, "general {sent:?}");
        // Keepalives carry run = 1 and count on from the bring-up.
        let hp = fake.received(3);
        assert!(hp.iter().all(|p| p[4] == 1));
        let seqs: Vec<u32> = hp
            .iter()
            .map(|p| u32::from_be_bytes([p[0], p[1], p[2], p[3]]))
            .collect();
        assert_eq!(seqs, (0..seqs.len() as u32).collect::<Vec<_>>());
    }

    #[test]
    fn drop_sends_run_zero() {
        let fake = FakeP2Radio::start();
        let src = open(&fake);
        fake.wait_for_run();
        drop(src);
        thread::sleep(Duration::from_millis(50));
        let hp = fake.received(3);
        assert!(hp.len() >= 4);
        let tail = &hp[hp.len() - 2..];
        assert!(
            tail.iter().all(|p| p[4] == 0),
            "last two high-priority have run = 0"
        );
        assert!(hp[..hp.len() - 2].iter().all(|p| p[4] == 1));
    }

    #[test]
    fn connection_reset_is_retried() {
        let e = |k| io::Error::new(k, "x");
        assert_eq!(
            classify_recv_error(&e(io::ErrorKind::ConnectionReset)),
            RecvErrorAction::Retry
        );
        assert_eq!(
            classify_recv_error(&e(io::ErrorKind::WouldBlock)),
            RecvErrorAction::Timeout
        );
        assert_eq!(
            classify_recv_error(&e(io::ErrorKind::TimedOut)),
            RecvErrorAction::Timeout
        );
        assert_eq!(
            classify_recv_error(&e(io::ErrorKind::PermissionDenied)),
            RecvErrorAction::Fatal
        );
    }

    #[test]
    fn validate_rejects_more_than_one_ddc_out_of_range_frequency_and_unencodable_rate() {
        let ok = HpsdrConfig {
            host: "192.0.2.10".to_string(),
            port: 1024,
            ddc_count: 1,
            sample_rate_hz: 192_000.0,
            center_freq_hz: vec![7_030_000.0],
            auto_attenuation: None,
        };
        assert_eq!(validate_protocol2(&ok).unwrap(), (192, 7_030_000.0));
        for rate in [48_000.0, 96_000.0, 384_000.0] {
            let cfg = HpsdrConfig {
                sample_rate_hz: rate,
                ..ok.clone()
            };
            assert!(validate_protocol2(&cfg).is_ok());
        }

        let two = HpsdrConfig {
            ddc_count: 2,
            center_freq_hz: vec![7_030_000.0, 14_000_000.0],
            ..ok.clone()
        };
        let err = validate_protocol2(&two).unwrap_err().to_string();
        assert!(err.contains("MAN-307"), "{err}");

        let high = HpsdrConfig {
            center_freq_hz: vec![70_000_000.0],
            ..ok.clone()
        };
        assert_eq!(
            validate_protocol2(&high).unwrap_err().to_string(),
            "HPSDR protocol 2: centre frequency 70000000 Hz is not below 61440000 Hz, the \
             radio's 122.88 MHz ADC Nyquist limit"
        );
        for bad in [0.0, -1.0, 7_030_000.5, f64::NAN, 61_440_000.0] {
            let cfg = HpsdrConfig {
                center_freq_hz: vec![bad],
                ..ok.clone()
            };
            assert!(validate_protocol2(&cfg).is_err(), "{bad}");
        }

        let rate = HpsdrConfig {
            sample_rate_hz: 250_000.0,
            ..ok.clone()
        };
        assert_eq!(
            validate_protocol2(&rate).unwrap_err().to_string(),
            "HPSDR protocol 2 only encodes exactly 48000/96000/192000/384000 Hz sample rates, \
             got 250000"
        );
    }

    /// MAN-423: an open source the caller has not read yet (a multi-input
    /// run parks it while another opener blocks) keeps ten datagrams, then
    /// its full socket drops 8,492 more -- past the 8,192 allowance. Read a
    /// second later, the drained backlog says nothing about how far the
    /// radio moved on: the jump is a loss counted from the pause, not a
    /// zero-length restart (before the fix: `Some(0)`, no dropped count).
    #[test]
    fn a_gap_past_the_allowance_after_a_park_is_a_loss() {
        let fake = FakeP2Radio::start();
        let mut src = open(&fake);
        for s in 0..10 {
            fake.send_iq(s);
        }
        thread::sleep(Duration::from_secs(1));
        fake.send_iq(10 + 8_492);
        let reads: Vec<_> = (0..11)
            .map(|_| {
                let (seq, _) = read_one(&mut src);
                (seq, src.take_missing_samples(), src.take_discontinuity())
            })
            .collect();
        assert!(
            reads[..10].iter().all(|r| r.1.is_none() && r.2.is_none()),
            "{reads:?}"
        );
        assert_eq!(reads[10], (10 + 8_492, None, Some(8_492 * 238)));
        let stats = src.gap_stats();
        assert_eq!(stats.dropped_packets, 8_492);
        assert_eq!(stats.gaps_detected, 1);
    }

    /// MAN-423: a radio restart behind the same park is measured from the
    /// pause, less the ten datagrams drained (before the fix: `Some(0)`).
    #[test]
    fn a_restart_behind_a_park_is_measured_from_the_pause() {
        let fake = FakeP2Radio::start();
        let before_open = Instant::now();
        let mut src = open(&fake);
        let after_open = Instant::now();
        for s in 0..10 {
            fake.send_iq(s);
        }
        thread::sleep(Duration::from_secs(1));
        fake.send_iq(0);
        let before_read = Instant::now();
        for _ in 0..10 {
            read_one(&mut src);
        }
        assert_eq!(read_one(&mut src), (0, 238));
        let after_read = Instant::now();
        let d = src
            .take_discontinuity()
            .expect("restart reports a discontinuity");
        let drained = Duration::from_secs_f64(10.0 * 238.0 / 192_000.0);
        assert_wall_clock_discontinuity(
            d,
            (before_read - after_open).saturating_sub(drained),
            (after_read - before_open).saturating_sub(drained),
        );
        assert_eq!(src.gap_stats().dropped_packets, 0);
    }

    /// MAN-423 review: a radio that restarts early in a pause keeps
    /// streaming into the socket, so the backlog holds five pre-restart
    /// datagrams, then a post-restart run 0..=119 with 20 lost to overflow.
    /// The restart's outage is the pause less what that run covers: every
    /// sample the source accounts for after the pause (read, missing or
    /// discontinuity) adds up to the pause's wall clock, not the pause plus
    /// the run (before the review fix: about 28,300 samples, the run's 119 datagrams,
    /// ahead).
    #[test]
    fn an_early_restart_behind_a_park_does_not_count_its_backlog_twice() {
        let fake = FakeP2Radio::start();
        let mut src = open(&fake);
        for s in 0..5 {
            fake.send_iq(s);
        }
        for _ in 0..4 {
            read_one(&mut src);
        }
        let before_pause = Instant::now();
        read_one(&mut src);
        let pause = Instant::now();
        for s in 5..10 {
            fake.send_iq(s);
        }
        thread::sleep(Duration::from_millis(20));
        for s in (0..60).chain(80..120) {
            fake.send_iq(s);
        }
        thread::sleep(Duration::from_millis(500));
        let before_read = Instant::now();
        let mut accounted = 0u64;
        let mut discontinuities = Vec::new();
        for _ in 0..105 {
            let (_, n) = read_one(&mut src);
            accounted += n as u64;
            accounted += src.take_missing_samples().unwrap_or(0);
            if let Some(d) = src.take_discontinuity() {
                discontinuities.push(d);
                accounted += d;
            }
        }
        let after_read = Instant::now();
        assert_eq!(discontinuities.len(), 1, "{discontinuities:?}");
        let stats = src.gap_stats();
        assert_eq!(stats.dropped_packets, 20);
        assert_eq!(stats.gaps_detected, 2);
        assert_wall_clock_discontinuity(accounted, before_read - pause, after_read - before_pause);
    }

    /// MAN-423 review: two restarts in one pause queue two runs, 0..60 and
    /// 0..60. The first restart's look-ahead holds both, and both are radio
    /// time, so the pause is assigned once: everything accounted after it
    /// adds up to its wall clock (before: the second run, 14,280 samples,
    /// ahead), and the second restart reports no more than the drift.
    #[test]
    fn two_restarts_behind_a_park_assign_the_pause_once() {
        let fake = FakeP2Radio::start();
        let mut src = open(&fake);
        for s in 0..5 {
            fake.send_iq(s);
        }
        for _ in 0..4 {
            read_one(&mut src);
        }
        let before_pause = Instant::now();
        read_one(&mut src);
        let pause = Instant::now();
        for s in 5..10 {
            fake.send_iq(s);
        }
        thread::sleep(Duration::from_millis(20));
        for s in 0..60 {
            fake.send_iq(s);
        }
        thread::sleep(Duration::from_millis(20));
        for s in 0..60 {
            fake.send_iq(s);
        }
        thread::sleep(Duration::from_millis(500));
        let before_read = Instant::now();
        let mut accounted = 0u64;
        let mut discontinuities = Vec::new();
        for _ in 0..125 {
            let (_, n) = read_one(&mut src);
            accounted += n as u64;
            accounted += src.take_missing_samples().unwrap_or(0);
            if let Some(d) = src.take_discontinuity() {
                discontinuities.push(d);
                accounted += d;
            }
        }
        let after_read = Instant::now();
        assert_eq!(discontinuities.len(), 2, "{discontinuities:?}");
        // The second restart finds the pause already assigned: what is left
        // is the time spent reading, well under one 10 ms datagram budget.
        assert!(discontinuities[1] < 1_920, "{discontinuities:?}");
        assert_eq!(src.gap_stats().gaps_detected, 2);
        assert_eq!(src.gap_stats().dropped_packets, 0);
        assert_wall_clock_discontinuity(accounted, before_read - pause, after_read - before_pause);
    }

    /// MAN-423 review: a second pause while the first park's backlog still
    /// drains keeps the first anchor. A 4 s park (the review's 15 s, scaled
    /// for test time), half the 100-datagram backlog drained, a 200 ms
    /// pause, the rest drained: an 11,000-datagram jump then fits the
    /// 4 s bound (about 11,400 datagrams) and is a loss of 2,618,000
    /// samples. Before the fix the second pause re-anchored the window, the
    /// bound fell to about 0.2 s (8,355 datagrams), and the jump read as a
    /// restart of a few tens of thousands of samples.
    #[test]
    fn a_second_pause_while_a_park_drains_keeps_its_anchor() {
        let fake = FakeP2Radio::start();
        let mut src = open(&fake);
        fake.send_iq(0);
        read_one(&mut src);
        thread::sleep(Duration::from_secs(4));
        for s in 1..101 {
            fake.send_iq(s);
        }
        for _ in 0..50 {
            read_one(&mut src);
        }
        thread::sleep(Duration::from_millis(200));
        for _ in 0..50 {
            read_one(&mut src);
        }
        let jump = 101 + 11_000;
        fake.send_iq(jump);
        assert_eq!(read_one(&mut src), (jump, 238));
        assert_eq!(src.take_missing_samples(), None);
        assert_eq!(src.take_discontinuity(), Some(11_000 * 238));
        let stats = src.gap_stats();
        assert_eq!(stats.dropped_packets, 11_000);
        assert_eq!(stats.gaps_detected, 1);
    }

    /// MAN-423: a source read without pause keeps the fixed allowance: a
    /// jump 2,000 past it is still a restart, whatever the backlog held.
    #[test]
    fn a_drained_source_keeps_the_fixed_allowance() {
        let fake = FakeP2Radio::start();
        let mut src = open(&fake);
        let seqs: Vec<u32> = (0..40).chain([40 + 10_192]).collect();
        let reads = feed(&fake, &mut src, &seqs);
        let last = reads.last().unwrap();
        assert_eq!((last.0, last.1), (40 + 10_192, None));
        assert!(last.2.expect("a restart") < 192_000, "{last:?}");
        assert_eq!(src.gap_stats().dropped_packets, 0);
    }

    /// MAN-423: a pause's window ends once its backlog is surely drained
    /// (`park_window` accepted datagrams); a later jump past the allowance
    /// is a restart again.
    #[test]
    fn a_park_window_ends_after_its_datagrams() {
        let fake = FakeP2Radio::start();
        let mut src = open(&fake);
        src.park_window = 20;
        for s in 0..10 {
            fake.send_iq(s);
        }
        thread::sleep(Duration::from_secs(1));
        for _ in 0..10 {
            read_one(&mut src);
        }
        let seqs: Vec<u32> = (10..30).chain([30 + 8_492]).collect();
        let reads = feed(&fake, &mut src, &seqs);
        let last = reads.last().unwrap();
        assert_eq!((last.0, last.1), (30 + 8_492, None));
        assert!(last.2.expect("a restart") < 192_000, "{last:?}");
        assert_eq!(src.gap_stats().dropped_packets, 0);
    }

    /// MAN-52: `MANTA_TEST_P2_HOST=<radio> cargo test -p manta-input --locked
    /// -- --ignored streams_iq_from_a_real_protocol2_radio`.
    #[test]
    #[ignore = "needs a protocol-2 radio: set MANTA_TEST_P2_HOST"]
    fn streams_iq_from_a_real_protocol2_radio() {
        let host = std::env::var("MANTA_TEST_P2_HOST").expect("set MANTA_TEST_P2_HOST");
        let cfg = HpsdrConfig {
            host,
            port: super::super::CONTROL_PORT,
            ddc_count: 1,
            sample_rate_hz: 192_000.0,
            center_freq_hz: vec![14_000_000.0],
            auto_attenuation: None,
        };
        let mut src = Protocol2IqSource::open(&cfg, wire::board_info(None)).unwrap();
        let mut buf = vec![Complex32::new(0.0, 0.0); BIG];
        let mut total = 0usize;
        while total < 5 * 192_000 {
            total += src.read(&mut buf).unwrap();
        }
        assert_eq!(src.gap_stats().malformed_packets, 0);
        assert!(src.confirmed_live_handle().unwrap().load(Ordering::SeqCst));
        let _ = SocketAddr::from(([0, 0, 0, 0], 0));
    }
}
