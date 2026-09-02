//! OpenHPSDR/Hermes (Protocol 1/"Metis") network IQ source: Hermes-Lite 2,
//! Pavel Demin's `sdr_receiver_hpsdr`/`sdr_transceiver_hpsdr` Red Pitaya
//! images, and his QMTech+`qmtech-adc` board. ARCHITECTURE §3.
//!
//! Feature-gated `hpsdr`, matching how `soapy` is feature-gated — this
//! module has no native-library dependency (pure UDP/std), but the gate is
//! kept for symmetry with the other network/hardware sources and so a build
//! can opt it out.
//!
//! Wire facts below are pinned by
//! `docs/DECISIONS/2026-09-02-hpsdr-hermes-protocol-spike.md` (MAN-10),
//! itself doc/source-verified against piHPSDR's `old_protocol.c` — **not**
//! against live hardware (none was reachable in that session or this one).
//! Per that doc's own "Needs real hardware" section, MAN-11's acceptance
//! criteria require confirming these against George's (K5TR) actual units
//! before merge:
//!
//! - Metis packet framing, IQ demux stride, and 24-bit sample decoding
//!   (below) — **spike-pinned**, high confidence.
//! - The exact C&C "start streaming" command byte layout sent by
//!   [`HpsdrDevice::open`] — this module's best-effort encoding of the
//!   publicly documented OpenHPSDR "General"/Start-Stop packet, **not
//!   independently re-verified against real hardware in this session**.
//!   If a real device doesn't start streaming, this is the first place to
//!   check.
//! - The keepalive strategy (periodic re-send of the same start command) —
//!   an implementation choice addressing the HL2 skimmer-gateware watchdog
//!   issue the spike documents, not itself a pinned protocol fact.
//!
//! ## Per-DDC demux (Metis packet layout)
//!
//! One UDP flow carries every configured receiver's IQ, time-multiplexed:
//! 1032-byte Metis packet = 8-byte Metis header + two 512-byte USB frames.
//! Each USB frame = 8-byte sub-header (3 sync bytes `0x7F 0x7F 0x7F` + C0-C4
//! control bytes) + 504 bytes of payload. Within the payload, samples are
//! strided `num_receivers * 6 + 2` bytes apart (24-bit I + 24-bit Q per
//! receiver, plus a fixed 2-byte mic/aux slot regardless of receiver
//! count) — the demux stride depends on the configured receiver count, not
//! on anything derivable per-packet in isolation.
//!
//! ## Loss/reorder handling
//!
//! Protocol 1 carries **no per-DDC (or general per-packet) sequence
//! number** usable for loss detection — see the spike doc. Gap detection
//! here is therefore cadence-based: given the configured sample rate and
//! receiver count, the expected inter-packet arrival interval is known, and
//! [`GapDetector`] flags an arrival that's late enough to indicate a missed
//! packet. Because a dropped/reordered Metis packet carries every
//! configured receiver's samples for that instant, a detected gap applies
//! to all DDCs demuxed from this device's stream at once — there is no way
//! to attribute a gap to one specific receiver channel in Protocol 1.
//!
//! ## DDC-count/rate cap
//!
//! One 100 Mbps Ethernet link supports roughly 8 simultaneous 192 kHz DDCs
//! before saturating (spike doc, "Bandwidth math"). [`validate_ddc_config`]
//! enforces this so a device whose gateware exceeds the per-link budget
//! (e.g. HL2's 9-10 DDC skimmer gateware at 192 kHz) is rejected at connect
//! time with an actionable error, rather than silently overrunning the
//! link.

use crate::IqSource;
use anyhow::{bail, Context, Result};
use num_complex::Complex32;
use std::collections::VecDeque;
use std::net::UdpSocket;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Metis/Hermes discovery and control port. Spike doc: "standard Metis
/// broadcast, UDP port 1024" for HL2; Red-Pitaya-family devices are reached
/// the same way once their IP is known (this driver does not implement the
/// separate avahi/mDNS discovery path the spike flags for that family —
/// callers supply the host directly, matching `KiwiIqSource::connect`'s
/// host/port convention).
pub const CONTROL_PORT: u16 = 1024;

const METIS_HEADER_LEN: usize = 8;
const USB_FRAME_LEN: usize = 512;
const USB_SUBHEADER_LEN: usize = 8;
const USB_PAYLOAD_LEN: usize = USB_FRAME_LEN - USB_SUBHEADER_LEN;
const USB_FRAMES_PER_PACKET: usize = 2;
const METIS_PACKET_LEN: usize = METIS_HEADER_LEN + USB_FRAMES_PER_PACKET * USB_FRAME_LEN;
const USB_SYNC: [u8; 3] = [0x7F, 0x7F, 0x7F];
const IQ_BYTES_PER_SAMPLE: usize = 6;
const MIC_AUX_BYTES: usize = 2;
/// 2^23 - 1: the 24-bit signed full-scale divisor reference clients use to
/// normalize HPSDR IQ samples (spike doc).
const IQ_FULL_SCALE: f32 = 8_388_607.0;

/// UDP read timeout: bounds how long a single socket read blocks, so
/// `read()` stays responsive to repeated calls rather than hanging forever
/// on a stalled link. Matches `KiwiIqSource`'s `READ_TIMEOUT` convention.
const READ_TIMEOUT: Duration = Duration::from_millis(250);

/// Bound on consecutive read timeouts before `read()` gives up with a real
/// `Err` instead of hanging indefinitely. Matches `KiwiIqSource`'s
/// `MAX_CONSECUTIVE_TIMEOUTS` convention (~10 s at 250 ms/timeout).
const MAX_CONSECUTIVE_TIMEOUTS: u32 = 40;

/// How often the start command is re-sent as a keepalive. The spike doc
/// flags the HL2 skimmer gateware's FPGA watchdog as timing out without
/// periodic host traffic; re-sending the start command is this driver's
/// (unverified against real hardware) answer to that, chosen because it
/// needs no additional undocumented command type.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(1);

/// One 100 Mbps Ethernet link's practical DDC budget at 192 kHz: 8 DDCs x
/// ~9.8 Mbps/DDC on-wire (spike doc "Bandwidth math" — 9.216 Mbps raw + ~6%
/// framing overhead), leaving headroom rather than running the link at
/// 95-100% utilization.
pub const LINK_BUDGET_MBPS: f64 = 78.4;

/// On-wire bandwidth (Mbps) for `ddc_count` receivers at `sample_rate_hz`,
/// including the spike doc's ~6% Metis/UDP/IP/Ethernet framing overhead
/// estimate. Protocol 2 would reframe this, not compress it — the raw IQ
/// bit rate is a link-layer constant independent of protocol dialect.
pub fn on_wire_mbps(sample_rate_hz: f64, ddc_count: usize) -> f64 {
    const WIRE_OVERHEAD: f64 = 1.06;
    sample_rate_hz * IQ_BYTES_PER_SAMPLE as f64 * 8.0 * ddc_count as f64 / 1e6 * WIRE_OVERHEAD
}

/// Reject a receiver-count/sample-rate combination that would exceed one
/// 100 Mbps link's practical budget ([`LINK_BUDGET_MBPS`]) — e.g. HL2's
/// 9-10 DDC skimmer gateware at 192 kHz. Callers exceeding this need to cap
/// DDC count, drop the per-DDC rate, or split across a second physical
/// link (spike doc).
pub fn validate_ddc_config(sample_rate_hz: f64, ddc_count: usize) -> Result<()> {
    if ddc_count == 0 {
        bail!("HPSDR device needs at least one configured receiver (DDC)");
    }
    let mbps = on_wire_mbps(sample_rate_hz, ddc_count);
    if mbps > LINK_BUDGET_MBPS {
        bail!(
            "{ddc_count} DDC(s) at {sample_rate_hz:.0} Hz needs ~{mbps:.1} Mbps, exceeding the \
             ~{LINK_BUDGET_MBPS:.1} Mbps practical ceiling for one 100 Mbps link \
             (docs/DECISIONS/2026-09-02-hpsdr-hermes-protocol-spike.md); reduce DDC count or \
             sample rate, or split across a second physical link"
        );
    }
    Ok(())
}

/// Complex samples per receiver per USB frame: `floor(504 / (num_receivers *
/// 6 + 2))` (spike doc — the fixed `+2` mic/aux slot applies regardless of
/// receiver count).
pub fn samples_per_usb_frame(num_receivers: usize) -> usize {
    if num_receivers == 0 {
        return 0;
    }
    USB_PAYLOAD_LEN / (num_receivers * IQ_BYTES_PER_SAMPLE + MIC_AUX_BYTES)
}

/// Decode a big-endian 24-bit two's-complement sample, normalized to
/// roughly [-1, 1] by `IQ_FULL_SCALE` (spike doc's `/8388607.0` convention).
fn decode_i24_be(b: &[u8]) -> f32 {
    let raw = ((b[0] as i32) << 16) | ((b[1] as i32) << 8) | (b[2] as i32);
    let signed = if raw & 0x0080_0000 != 0 {
        raw - 0x0100_0000
    } else {
        raw
    };
    signed as f32 / IQ_FULL_SCALE
}

/// Demux one 504-byte USB-frame payload into `num_receivers` independent
/// per-DDC complex-sample streams, appending to `out[receiver_index]`.
fn demux_usb_payload(payload: &[u8], num_receivers: usize, out: &mut [Vec<Complex32>]) {
    let stride = num_receivers * IQ_BYTES_PER_SAMPLE + MIC_AUX_BYTES;
    let n_frames = samples_per_usb_frame(num_receivers);
    for frame_idx in 0..n_frames {
        let base = frame_idx * stride;
        for (rx, samples) in out.iter_mut().enumerate().take(num_receivers) {
            let off = base + rx * IQ_BYTES_PER_SAMPLE;
            let i = decode_i24_be(&payload[off..off + 3]);
            let q = decode_i24_be(&payload[off + 3..off + 6]);
            samples.push(Complex32::new(i, q));
        }
    }
}

/// Demux one full 1032-byte Metis packet into `num_receivers` per-DDC
/// complex-sample streams, appending to `out[receiver_index]`.
/// `out.len()` must be >= `num_receivers`.
pub fn demux_metis_packet(
    packet: &[u8],
    num_receivers: usize,
    out: &mut [Vec<Complex32>],
) -> Result<()> {
    if packet.len() != METIS_PACKET_LEN {
        bail!(
            "Metis packet must be {METIS_PACKET_LEN} bytes, got {}",
            packet.len()
        );
    }
    if out.len() < num_receivers {
        bail!(
            "demux output has {} DDC slot(s), need {num_receivers}",
            out.len()
        );
    }
    for frame in 0..USB_FRAMES_PER_PACKET {
        let frame_start = METIS_HEADER_LEN + frame * USB_FRAME_LEN;
        let sync = &packet[frame_start..frame_start + 3];
        if sync != USB_SYNC {
            bail!("USB frame {frame} has bad sync bytes {sync:02x?}, expected {USB_SYNC:02x?}");
        }
        let payload_start = frame_start + USB_SUBHEADER_LEN;
        let payload = &packet[payload_start..payload_start + USB_PAYLOAD_LEN];
        demux_usb_payload(payload, num_receivers, out);
    }
    Ok(())
}

/// Cadence-based UDP loss/reorder detector for a Protocol 1 stream that
/// carries no per-packet sequence number (module docs). Flags an arrival
/// whose gap since the last packet exceeds `tolerance` x the expected
/// inter-packet interval.
pub struct GapDetector {
    expected_interval: Duration,
    tolerance: f64,
    last_arrival: Option<Instant>,
    dropped_packets: u64,
    gaps_detected: u64,
}

impl GapDetector {
    /// `tolerance` of 1.5 means an arrival more than 1.5x the expected
    /// interval late is flagged as a probable dropped packet.
    pub fn new(sample_rate_hz: f64, num_receivers: usize, tolerance: f64) -> Self {
        let samples_per_packet = USB_FRAMES_PER_PACKET * samples_per_usb_frame(num_receivers);
        let expected_interval = if samples_per_packet == 0 || sample_rate_hz <= 0.0 {
            Duration::from_secs(1)
        } else {
            Duration::from_secs_f64(samples_per_packet as f64 / sample_rate_hz)
        };
        GapDetector {
            expected_interval,
            tolerance,
            last_arrival: None,
            dropped_packets: 0,
            gaps_detected: 0,
        }
    }

    /// Record a packet's arrival at `now`, returning `true` if this arrival
    /// closed a detected gap.
    pub fn observe(&mut self, now: Instant) -> bool {
        let mut flagged = false;
        if let Some(last) = self.last_arrival {
            let elapsed = now.saturating_duration_since(last);
            let threshold = self.expected_interval.mul_f64(self.tolerance);
            if elapsed > threshold && self.expected_interval > Duration::ZERO {
                let missed =
                    (elapsed.as_secs_f64() / self.expected_interval.as_secs_f64()).round() as u64;
                self.dropped_packets += missed.saturating_sub(1).max(1);
                self.gaps_detected += 1;
                flagged = true;
            }
        }
        self.last_arrival = Some(now);
        flagged
    }

    pub fn stats(&self) -> GapStats {
        GapStats {
            dropped_packets: self.dropped_packets,
            gaps_detected: self.gaps_detected,
        }
    }
}

/// Loss/reorder counters for a device stream (module docs: "gap is visible
/// in metrics"). Matching `manta-engine::soak`'s documented deviation, this
/// is exposed as a plain getter rather than wired into a project-wide
/// metrics system, which doesn't exist yet for live-hardware sources.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GapStats {
    pub dropped_packets: u64,
    pub gaps_detected: u64,
}

/// Configuration for one HPSDR/Hermes device connection.
#[derive(Debug, Clone)]
pub struct HpsdrConfig {
    pub host: String,
    pub port: u16,
    /// Number of receiver channels (DDCs) to configure. Validated against
    /// [`LINK_BUDGET_MBPS`] by [`validate_ddc_config`].
    pub ddc_count: usize,
    pub sample_rate_hz: f64,
    /// Per-DDC center frequency, reported by the corresponding
    /// `HpsdrIqSource::center_freq_hz()` (metadata only — see module docs;
    /// this driver does not itself send per-receiver tuning words).
    pub center_freq_hz: Vec<f64>,
}

impl HpsdrConfig {
    pub fn validate(&self) -> Result<()> {
        validate_ddc_config(self.sample_rate_hz, self.ddc_count)?;
        if self.center_freq_hz.len() != self.ddc_count {
            bail!(
                "center_freq_hz has {} entries, need one per DDC ({})",
                self.center_freq_hz.len(),
                self.ddc_count
            );
        }
        Ok(())
    }
}

/// Best-effort encoding of the publicly documented OpenHPSDR "General"
/// start/stop C&C packet: `0xEF 0xFE` sync, command byte `0x04`, then C0
/// with bit0 set to start streaming, remaining bytes zero-padded to the
/// conventional 60-byte "General" packet length. **Not independently
/// re-verified against real hardware in this session** — see module docs.
fn build_start_command() -> [u8; 60] {
    let mut pkt = [0u8; 60];
    pkt[0] = 0xEF;
    pkt[1] = 0xFE;
    pkt[2] = 0x04;
    pkt[3] = 0x01; // C0: run=1, IQ-only (bits 1-2 = 00)
    pkt
}

struct Inner {
    socket: UdpSocket,
    queues: Vec<VecDeque<Complex32>>,
    gap: GapDetector,
    num_receivers: usize,
    last_keepalive: Instant,
}

impl Inner {
    /// Receive and demux exactly one Metis packet, filling every DDC's
    /// queue and updating the gap detector. All consumer threads share
    /// this single socket, so whichever `HpsdrIqSource::read` finds its own
    /// queue empty drives the pump.
    fn pump_one_packet(&mut self) -> Result<()> {
        self.send_keepalive_if_due()?;

        let mut consecutive_timeouts = 0u32;
        let packet = loop {
            let mut buf = [0u8; METIS_PACKET_LEN];
            match self.socket.recv(&mut buf) {
                Ok(n) if n == METIS_PACKET_LEN => break buf,
                Ok(_) => continue, // short/oversized datagram; not a Metis IQ packet
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    consecutive_timeouts += 1;
                    if consecutive_timeouts >= MAX_CONSECUTIVE_TIMEOUTS {
                        bail!(
                            "HPSDR device stalled: no data for {} consecutive read timeouts",
                            MAX_CONSECUTIVE_TIMEOUTS
                        );
                    }
                    continue;
                }
                Err(e) => return Err(e).context("HPSDR UDP read"),
            }
        };

        self.gap.observe(Instant::now());
        let mut demuxed = vec![Vec::new(); self.num_receivers];
        demux_metis_packet(&packet, self.num_receivers, &mut demuxed)?;
        for (queue, samples) in self.queues.iter_mut().zip(demuxed) {
            queue.extend(samples);
        }
        Ok(())
    }

    fn send_keepalive_if_due(&mut self) -> Result<()> {
        if self.last_keepalive.elapsed() >= KEEPALIVE_INTERVAL {
            self.socket
                .send(&build_start_command())
                .context("send HPSDR keepalive")?;
            self.last_keepalive = Instant::now();
        }
        Ok(())
    }
}

/// A live HPSDR/Hermes device connection, demultiplexing one shared UDP
/// stream into independent per-DDC [`IqSource`] handles.
pub struct HpsdrDevice;

impl HpsdrDevice {
    /// Connect to `cfg.host:cfg.port`, send the start command, and return
    /// one [`HpsdrIqSource`] per configured DDC. All returned sources share
    /// the same underlying socket and demux state (`Arc<Mutex<..>>`) so
    /// they may be moved to independent threads, matching how
    /// `manta-engine` runs each source. ARCHITECTURE §3.
    pub fn open(cfg: HpsdrConfig) -> Result<Vec<HpsdrIqSource>> {
        cfg.validate()?;

        let socket = UdpSocket::bind("0.0.0.0:0").context("bind HPSDR UDP socket")?;
        socket
            .set_read_timeout(Some(READ_TIMEOUT))
            .context("set HPSDR UDP read timeout")?;
        socket
            .connect((cfg.host.as_str(), cfg.port))
            .with_context(|| format!("connect HPSDR UDP socket to {}:{}", cfg.host, cfg.port))?;
        socket
            .send(&build_start_command())
            .context("send HPSDR start command")?;

        let inner = Arc::new(Mutex::new(Inner {
            socket,
            queues: vec![VecDeque::new(); cfg.ddc_count],
            gap: GapDetector::new(cfg.sample_rate_hz, cfg.ddc_count, 1.5),
            num_receivers: cfg.ddc_count,
            last_keepalive: Instant::now(),
        }));

        Ok((0..cfg.ddc_count)
            .map(|ddc_index| HpsdrIqSource {
                inner: inner.clone(),
                ddc_index,
                sample_rate_hz: cfg.sample_rate_hz,
                center_freq_hz: cfg.center_freq_hz[ddc_index],
            })
            .collect())
    }
}

/// One receiver channel (DDC) of an [`HpsdrDevice`] connection, as an
/// independent `IqSource`. ARCHITECTURE §3.
pub struct HpsdrIqSource {
    inner: Arc<Mutex<Inner>>,
    ddc_index: usize,
    sample_rate_hz: f64,
    center_freq_hz: f64,
}

impl HpsdrIqSource {
    /// Loss/reorder counters observed on this device's shared stream so
    /// far (module docs: a gap applies to every DDC demuxed from the
    /// affected packet, so these counters are shared across all of a
    /// device's `HpsdrIqSource` handles).
    pub fn gap_stats(&self) -> GapStats {
        self.inner.lock().unwrap().gap.stats()
    }
}

impl IqSource for HpsdrIqSource {
    fn sample_rate(&self) -> f64 {
        self.sample_rate_hz
    }

    fn center_freq_hz(&self) -> f64 {
        self.center_freq_hz
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        let mut inner = self.inner.lock().unwrap();
        while inner.queues[self.ddc_index].is_empty() {
            inner.pump_one_packet()?;
        }
        let queue = &mut inner.queues[self.ddc_index];
        let n = buf.len().min(queue.len());
        for slot in buf.iter_mut().take(n) {
            *slot = queue.pop_front().expect("checked len above");
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket as StdUdpSocket;

    fn encode_i24_be(sample: f32) -> [u8; 3] {
        let raw = (sample * IQ_FULL_SCALE).round() as i32;
        let raw = raw & 0x00FF_FFFF;
        [(raw >> 16) as u8, (raw >> 8) as u8, raw as u8]
    }

    /// Build one synthetic Metis packet with `num_receivers` receivers,
    /// each USB frame filled with a distinct, easily-checked IQ value per
    /// receiver so demux correctness (and per-receiver attribution) can be
    /// verified byte-for-byte, mirroring `kiwi.rs`'s synthetic-frame tests.
    fn synth_metis_packet(num_receivers: usize, value_for: impl Fn(usize) -> Complex32) -> Vec<u8> {
        let mut pkt = vec![0u8; METIS_PACKET_LEN];
        let stride = num_receivers * IQ_BYTES_PER_SAMPLE + MIC_AUX_BYTES;
        let n_frames = samples_per_usb_frame(num_receivers);
        for frame in 0..USB_FRAMES_PER_PACKET {
            let frame_start = METIS_HEADER_LEN + frame * USB_FRAME_LEN;
            pkt[frame_start..frame_start + 3].copy_from_slice(&USB_SYNC);
            let payload_start = frame_start + USB_SUBHEADER_LEN;
            for f in 0..n_frames {
                let base = payload_start + f * stride;
                for rx in 0..num_receivers {
                    let off = base + rx * IQ_BYTES_PER_SAMPLE;
                    let s = value_for(rx);
                    pkt[off..off + 3].copy_from_slice(&encode_i24_be(s.re));
                    pkt[off + 3..off + 6].copy_from_slice(&encode_i24_be(s.im));
                }
            }
        }
        pkt
    }

    #[test]
    fn samples_per_usb_frame_matches_spike_stride_math() {
        // 504 / (1*6+2) = 63
        assert_eq!(samples_per_usb_frame(1), 63);
        // 504 / (2*6+2) = 36
        assert_eq!(samples_per_usb_frame(2), 36);
        // 504 / (8*6+2) = 10
        assert_eq!(samples_per_usb_frame(8), 10);
        assert_eq!(samples_per_usb_frame(0), 0);
    }

    #[test]
    fn decodes_i24_be_positive_and_negative() {
        assert!((decode_i24_be(&[0x00, 0x00, 0x01]) - 1.0 / IQ_FULL_SCALE).abs() < 1e-9);
        assert!((decode_i24_be(&[0x7F, 0xFF, 0xFF]) - 1.0).abs() < 1e-6);
        assert!((decode_i24_be(&[0xFF, 0xFF, 0xFF]) - (-1.0 / IQ_FULL_SCALE)).abs() < 1e-9);
        assert!((decode_i24_be(&[0x80, 0x00, 0x00]) - (-1.0)).abs() < 1e-6);
    }

    #[test]
    fn demux_attributes_samples_to_the_right_ddc() {
        let num_receivers = 3;
        let values = [
            Complex32::new(0.25, -0.25),
            Complex32::new(0.5, 0.1),
            Complex32::new(-0.75, 0.9),
        ];
        let pkt = synth_metis_packet(num_receivers, |rx| values[rx]);
        let mut out = vec![Vec::new(); num_receivers];
        demux_metis_packet(&pkt, num_receivers, &mut out).unwrap();

        let expected_per_frame = USB_FRAMES_PER_PACKET * samples_per_usb_frame(num_receivers);
        for (rx, samples) in out.iter().enumerate() {
            assert_eq!(samples.len(), expected_per_frame);
            for s in samples {
                assert!((s.re - values[rx].re).abs() < 1e-4, "rx={rx} re={}", s.re);
                assert!((s.im - values[rx].im).abs() < 1e-4, "rx={rx} im={}", s.im);
            }
        }
    }

    #[test]
    fn wrong_packet_length_is_a_clean_error() {
        let mut out = vec![Vec::new(); 1];
        assert!(demux_metis_packet(&[0u8; 100], 1, &mut out).is_err());
    }

    #[test]
    fn bad_usb_sync_is_a_clean_error() {
        let mut pkt = synth_metis_packet(1, |_| Complex32::new(0.0, 0.0));
        pkt[METIS_HEADER_LEN] = 0x00; // corrupt first USB frame's sync
        let mut out = vec![Vec::new(); 1];
        assert!(demux_metis_packet(&pkt, 1, &mut out).is_err());
    }

    #[test]
    fn on_wire_mbps_matches_spike_derivation() {
        // 8 DDCs @ 192 kHz: spike doc's own ~78 Mbps figure.
        let mbps = on_wire_mbps(192_000.0, 8);
        assert!((mbps - 78.15).abs() < 0.1, "got {mbps}");
    }

    #[test]
    fn validate_ddc_config_accepts_the_8_ddc_192khz_ceiling() {
        assert!(validate_ddc_config(192_000.0, 8).is_ok());
    }

    #[test]
    fn validate_ddc_config_rejects_hl2_skimmer_gateware_at_192khz() {
        // Spike doc: HL2's 9-10 DDC skimmer gateware exceeds the 100 Mbps
        // link ceiling at 192 kHz.
        assert!(validate_ddc_config(192_000.0, 9).is_err());
        assert!(validate_ddc_config(192_000.0, 10).is_err());
    }

    #[test]
    fn validate_ddc_config_allows_more_ddcs_at_lower_rates() {
        // Same 8-DDC-equivalent budget scales down proportionally at 48 kHz.
        assert!(validate_ddc_config(48_000.0, 32).is_ok());
        assert!(validate_ddc_config(48_000.0, 40).is_err());
    }

    #[test]
    fn validate_ddc_config_rejects_zero_ddcs() {
        assert!(validate_ddc_config(192_000.0, 0).is_err());
    }

    #[test]
    fn gap_detector_stays_quiet_on_steady_cadence() {
        let mut gap = GapDetector::new(192_000.0, 1, 1.5);
        let interval = gap.expected_interval;
        let mut now = Instant::now();
        for _ in 0..10 {
            assert!(!gap.observe(now));
            now += interval;
        }
        assert_eq!(gap.stats(), GapStats::default());
    }

    #[test]
    fn gap_detector_flags_a_missed_packet() {
        let mut gap = GapDetector::new(192_000.0, 1, 1.5);
        let interval = gap.expected_interval;
        let mut now = Instant::now();
        assert!(!gap.observe(now));
        now += interval; // one normal arrival
        assert!(!gap.observe(now));
        now += interval * 3; // a missed packet: arrives 3 intervals late
        assert!(gap.observe(now));
        let stats = gap.stats();
        assert_eq!(stats.gaps_detected, 1);
        assert!(stats.dropped_packets >= 1);
    }

    #[test]
    fn config_validate_checks_center_freq_len() {
        let cfg = HpsdrConfig {
            host: "127.0.0.1".into(),
            port: CONTROL_PORT,
            ddc_count: 2,
            sample_rate_hz: 192_000.0,
            center_freq_hz: vec![14_025_000.0], // only one, need two
        };
        assert!(cfg.validate().is_err());
    }

    /// Loopback integration test: a fake "device" thread sends real
    /// Metis-shaped packets to a bound UDP socket; `HpsdrDevice::open`
    /// connects to it and each returned per-DDC `IqSource::read()` yields
    /// the expected demuxed samples. No external hardware involved.
    #[test]
    fn reads_two_ddcs_from_a_loopback_device() {
        let device_socket = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        let device_addr = device_socket.local_addr().unwrap();
        device_socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        let cfg = HpsdrConfig {
            host: device_addr.ip().to_string(),
            port: device_addr.port(),
            ddc_count: 2,
            sample_rate_hz: 192_000.0,
            center_freq_hz: vec![14_025_000.0, 14_030_000.0],
        };
        let mut sources = HpsdrDevice::open(cfg).unwrap();
        assert_eq!(sources.len(), 2);

        // Consume the start command / any keepalive so it doesn't get
        // mistaken for an IQ packet, then reply with one real IQ packet.
        let mut discard = [0u8; METIS_PACKET_LEN];
        let (_, client_addr) = device_socket.recv_from(&mut discard).unwrap();

        let values = [Complex32::new(0.2, -0.3), Complex32::new(-0.4, 0.6)];
        let pkt = synth_metis_packet(2, |rx| values[rx]);
        device_socket.send_to(&pkt, client_addr).unwrap();

        let mut buf0 = vec![Complex32::new(0.0, 0.0); 4096];
        let n0 = sources[0].read(&mut buf0).unwrap();
        assert!(n0 > 0);
        assert!((buf0[0].re - values[0].re).abs() < 1e-4);
        assert!((buf0[0].im - values[0].im).abs() < 1e-4);

        let mut buf1 = vec![Complex32::new(0.0, 0.0); 4096];
        let n1 = sources[1].read(&mut buf1).unwrap();
        assert!(n1 > 0);
        assert!((buf1[0].re - values[1].re).abs() < 1e-4);
        assert!((buf1[0].im - values[1].im).abs() < 1e-4);

        assert_eq!(sources[0].sample_rate(), 192_000.0);
        assert_eq!(sources[0].center_freq_hz(), 14_025_000.0);
        assert_eq!(sources[1].center_freq_hz(), 14_030_000.0);
        assert_eq!(sources[0].gap_stats(), GapStats::default());
    }

    /// Real, live integration test against actual hardware. #[ignore]'d:
    /// no HPSDR device is reachable in this session (spike doc's "Needs
    /// real hardware" section) — run manually against George's HL2/Red
    /// Pitaya before this driver ships.
    #[test]
    #[ignore]
    fn connects_to_a_real_device_and_streams_iq() {
        let cfg = HpsdrConfig {
            host: "192.168.1.100".into(),
            port: CONTROL_PORT,
            ddc_count: 1,
            sample_rate_hz: 192_000.0,
            center_freq_hz: vec![14_025_000.0],
        };
        let mut sources = HpsdrDevice::open(cfg).expect("connect to a real HPSDR device");
        let mut buf = vec![Complex32::new(0.0, 0.0); 4096];
        let n = sources[0].read(&mut buf).expect("read real IQ samples");
        assert!(n > 0, "expected real samples from a live device");
    }
}
