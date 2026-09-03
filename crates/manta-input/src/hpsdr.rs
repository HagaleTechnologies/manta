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
//! - The C&C tuning packet fields (sample rate, receiver count, per-RX
//!   frequency) — the C0 ADDR/MOX bit layout and C1-C4 field encodings
//!   below are cross-confirmed against three independent sources (see
//!   `docs/DECISIONS/2026-09-03-man55-hpsdr-cc-tuning-protocol.md`,
//!   MAN-55). The outbound Metis packet's own header framing (reusing the
//!   inbound direction's already-pinned shape) is **not** independently
//!   re-verified, same caveat as the start command above.
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
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Bound on consecutive malformed datagrams (PR #75 review, round 1) before
/// `pump_one_packet` gives up with a real `Err`, the same way
/// `MAX_CONSECUTIVE_TIMEOUTS` bounds *silence*. Without this, a peer that
/// continuously sends length-correct-but-bad-sync-bytes packets (corrupt
/// hardware, or a spoofed stream) never trips the stall check -- that only
/// advances on `WouldBlock`/`TimedOut` -- so `pump_one_packet` loops
/// forever holding `Inner`'s mutex (blocking every other DDC's `read()` on
/// the same device), never re-checking the keepalive after its single
/// call at function entry, and consuming a core the whole time. No
/// wall-clock deadline is used here (unlike `READ_TIMEOUT`) because
/// malformed-packet arrival rate is attacker/peer-controlled and
/// unbounded, unlike a timeout's fixed cadence -- a count bound is what
/// actually caps worst-case work here.
const MAX_CONSECUTIVE_MALFORMED: u32 = 10_000;

/// Socket receive buffer size (PR #75 review, round 1): deliberately larger
/// than [`METIS_PACKET_LEN`], not equal to it. A buffer sized exactly to
/// the expected packet length can't tell a merely-oversized datagram apart
/// from a same-or-larger one that overflowed it: on POSIX, `recv()`
/// silently truncates an oversized datagram to fit and returns
/// `Ok(buf.len())` -- indistinguishable from a genuine `METIS_PACKET_LEN`
/// packet, so a garbage flood at exactly that length boundary could slip
/// past the wrong-length check entirely. On Windows, Winsock instead
/// returns a fatal `WSAEMSGSIZE` `Err` for the identical oversized
/// datagram, which used to propagate out of `pump_one_packet` as a real
/// I/O error and tear down the whole source -- undoing this file's own
/// oversized-datagram hardening on a platform this driver claims to
/// support (`AGENTS.md`). Sized to the maximum possible UDP payload
/// (65507 bytes, rounded up) so `recv()` can never truncate or hit
/// `WSAEMSGSIZE` for any datagram a peer could actually send, regardless
/// of platform -- an oversized datagram is now always visible as
/// `Ok(n)` with `n != METIS_PACKET_LEN`, handled by the same malformed
/// path as every other length mismatch.
const RECV_BUF_LEN: usize = 65536;

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
    malformed_packets: u64,
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
            malformed_packets: 0,
        }
    }

    /// Record a datagram discarded because it wasn't a valid Metis/Hermes IQ
    /// packet (wrong length, or a length-correct packet that failed sync/
    /// framing validation) — MAN-22: a malformed or adversarially-crafted
    /// UDP packet must be counted, not silently dropped.
    pub fn record_malformed(&mut self) {
        self.malformed_packets += 1;
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
            malformed_packets: self.malformed_packets,
        }
    }
}

/// Loss/reorder/malformed-packet counters for a device stream. Matching
/// `manta-engine::soak`'s documented deviation, this is exposed as a plain
/// getter rather than wired into a project-wide metrics system, which
/// doesn't exist yet for live-hardware sources (tracked as a MAN-22
/// follow-up: manta-input counters, `malformed_packets` included, aren't
/// reachable from manta-server's Prometheus `/metrics` endpoint yet).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GapStats {
    pub dropped_packets: u64,
    pub gaps_detected: u64,
    /// Datagrams discarded because they weren't a valid Metis/Hermes IQ
    /// packet: wrong length, or correct length but failed sync/framing
    /// validation (MAN-22).
    pub malformed_packets: u64,
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
        // Protocol 1's C&C address space only has RX-frequency slots for
        // 12 receivers (0x02-0x08, 0x12-0x16 -- MAN-55, see
        // cc_rx_freq_addr); validate_ddc_config's bandwidth check alone
        // permits more DDCs at low sample rates (e.g. 32 @ 48 kHz), which
        // Finding 1's tuning packets could never actually address.
        if self.ddc_count > 12 {
            bail!(
                "HPSDR Protocol 1 has C&C address space for at most 12 receivers, got {}",
                self.ddc_count
            );
        }
        // Fail fast at config-validate time rather than deep inside
        // HpsdrDevice::open's C&C-frame construction (MAN-55).
        cc_sample_rate_code(self.sample_rate_hz)?;
        // Every configured center frequency must be exactly representable
        // as the wire's unsigned 32-bit Hz value (PR #79 review, round 1,
        // P2) -- manta-cli's `--hpsdr-freq` parser only rejects
        // non-finite/non-positive values, so an out-of-range value (e.g.
        // > u32::MAX Hz) would otherwise reach `build_rx_freq_cc` deep
        // inside `HpsdrDevice::open`, which used to silently clamp it
        // instead of erroring -- caught here for every caller, not just
        // the CLI, and before any device is opened.
        for (i, &freq_hz) in self.center_freq_hz.iter().enumerate() {
            if !freq_hz.is_finite() || !(0.0..=u32::MAX as f64).contains(&freq_hz) {
                bail!(
                    "center_freq_hz[{i}] ({freq_hz}) is outside HPSDR Protocol 1's \
                     representable range (0..={} Hz)",
                    u32::MAX
                );
            }
        }
        Ok(())
    }
}

/// Sample-rate code for the ADDR=0x00 "General" C&C packet's `C1` bits
/// [1:0] (MAN-55, `docs/DECISIONS/2026-09-03-man55-hpsdr-cc-tuning-protocol.md`
/// — cross-confirmed against the Hermes-Lite2 protocol wiki and piHPSDR's
/// `old_protocol.c` `SPEED_*` macros). Requires an EXACT match against the
/// only four rates Protocol 1 can encode (PR #79 review, round 1, P2): a
/// tolerance-based near-match (e.g. 191999.75 Hz) used to silently
/// configure the device for 192 kHz while `HpsdrIqSource::sample_rate()`
/// kept reporting the original, subtly-wrong value -- `manta_engine`'s
/// `Channelizer::new` then rejects that value's `fs / 93.75` power-of-two
/// check only AFTER the device was already opened and calibration data
/// consumed. An unsupported or near-miss rate must fail loudly at
/// `HpsdrConfig::validate()` time instead.
fn cc_sample_rate_code(sample_rate_hz: f64) -> Result<u8> {
    if sample_rate_hz == 48_000.0 {
        Ok(0b00)
    } else if sample_rate_hz == 96_000.0 {
        Ok(0b01)
    } else if sample_rate_hz == 192_000.0 {
        Ok(0b10)
    } else if sample_rate_hz == 384_000.0 {
        Ok(0b11)
    } else {
        bail!(
            "HPSDR Protocol 1 only encodes exactly 48000/96000/192000/384000 Hz sample rates, \
             got {sample_rate_hz}"
        );
    }
}

/// C&C `ADDR` value for receiver `rx_index`'s (0-based) NCO frequency
/// field. Addresses 0x02-0x08 cover RX1-RX7; 0x09-0x11 is reserved for TX
/// drive/Alex/CW settings this RX-only driver never sends, so RX8-RX12
/// resume at 0x12-0x16, not 0x09 (MAN-55 decision doc, cross-confirmed
/// against the Hermes-Lite2 protocol wiki's address table).
fn cc_rx_freq_addr(rx_index: usize) -> Result<u8> {
    match rx_index {
        0..=6 => Ok(0x02 + rx_index as u8),
        7..=11 => Ok(0x12 + (rx_index - 7) as u8),
        _ => bail!("HPSDR Protocol 1 supports at most 12 receivers, got rx_index={rx_index}"),
    }
}

/// C0 byte: 6-bit `ADDR` in bits[6:1], `MOX` (transmit-active) in bit0
/// (MAN-55 decision doc -- cross-confirmed identically by the
/// Hermes-Lite2 protocol wiki, piHPSDR's sender `old_protocol.c`, and
/// piHPSDR's own protocol-simulator/decoder `hpsdrsim.c`: three
/// independent codebases agree bit-for-bit, the highest-confidence claim
/// in that research). This driver never transmits, so MOX is always 0.
fn cc_c0(addr: u8) -> u8 {
    (addr & 0x3F) << 1
}

/// Build the 5-byte C0-C4 "General" C&C header (`ADDR=0x00`) configuring
/// sample rate and receiver count -- the two device-side settings
/// `HpsdrConfig` needs actually applied rather than assumed (MAN-55
/// Finding 1). Other `ADDR=0x00` fields (antenna select, open-collector
/// outputs, duplex, ...) are left zero/default; this driver doesn't
/// expose them. Caller must have already validated `num_receivers` is in
/// 1..=12 and `sample_rate_hz` is encodable (`HpsdrConfig::validate`).
fn build_general_cc(sample_rate_hz: f64, num_receivers: usize) -> Result<[u8; 5]> {
    let rate_code = cc_sample_rate_code(sample_rate_hz)?;
    if num_receivers == 0 || num_receivers > 12 {
        bail!("HPSDR Protocol 1 supports 1-12 receivers, got {num_receivers}");
    }
    let mut cc = [0u8; 5];
    cc[0] = cc_c0(0x00);
    cc[1] = rate_code; // C1 bits[1:0]
    cc[4] = ((num_receivers as u8 - 1) & 0x0F) << 3; // C4 bits[6:3]
    Ok(cc)
}

/// Build the 5-byte C0-C4 header tuning receiver `rx_index` (0-based) to
/// `freq_hz`: a big-endian 32-bit Hz value across C1-C4, no scaling
/// (MAN-55 decision doc -- cross-confirmed against the wiki's
/// `DATA[31:24]=C1..DATA[7:0]=C4` bit-range table and piHPSDR's literal
/// `output_buffer[C1]=freq>>24` shift chain). Rejects (rather than
/// silently clamping) a frequency outside the wire's representable range
/// (PR #79 review, round 1, P2): a caller passing e.g. `u32::MAX + 1` Hz
/// used to have the tuning word silently saturate to `u32::MAX` while
/// `HpsdrIqSource::center_freq_hz()` kept reporting the original,
/// unclamped value, so a published spot's frequency would disagree with
/// what the hardware was actually tuned to.
fn build_rx_freq_cc(rx_index: usize, freq_hz: f64) -> Result<[u8; 5]> {
    let addr = cc_rx_freq_addr(rx_index)?;
    if !freq_hz.is_finite() || !(0.0..=u32::MAX as f64).contains(&freq_hz) {
        bail!(
            "HPSDR Protocol 1 encodes RX frequency as an unsigned 32-bit Hz value in \
             0..={}, got {freq_hz}",
            u32::MAX
        );
    }
    let freq = freq_hz.round() as u32;
    let mut cc = [0u8; 5];
    cc[0] = cc_c0(addr);
    cc[1..5].copy_from_slice(&freq.to_be_bytes());
    Ok(cc)
}

/// Embed one 5-byte C0-C4 C&C header into an otherwise-zeroed 512-byte USB
/// frame (sync + header; no mic/TX-audio payload since this driver is
/// RX-only).
fn build_cc_usb_frame(cc: [u8; 5]) -> [u8; USB_FRAME_LEN] {
    let mut frame = [0u8; USB_FRAME_LEN];
    frame[0..3].copy_from_slice(&USB_SYNC);
    frame[3..8].copy_from_slice(&cc);
    frame
}

/// Build one outbound (host -> device) Metis "C&C" packet carrying two
/// C&C USB frames: `0xEF 0xFE` sync, byte 2 = `0x01` (the USB-data-packet
/// type identifier), byte 3 = `0x02` (endpoint 2, host -> device), then
/// the 4-byte sequence number in bytes 4-7 (PR #79 review, round 1, P1 --
/// the original encoding put `0x02` directly in byte 2 and shifted the
/// sequence into bytes 3-6, leaving byte 7 always zero; every real
/// Protocol 1 EP2 write starts `EF FE 01 02` followed by the sequence, so
/// hardware would never have recognized the prior encoding as a valid
/// C&C packet at all). Reuses this file's already-pinned inbound Metis
/// packet framing (8-byte header + two 512-byte USB frames) for the send
/// direction, mirroring `build_start_command`'s use of `0x04` for the
/// distinct start/stop command. Still **not independently re-verified
/// against real hardware** (MAN-55 decision doc), same caveat as
/// `build_start_command`.
fn build_cc_packet(
    seq: u32,
    frame_a: [u8; USB_FRAME_LEN],
    frame_b: [u8; USB_FRAME_LEN],
) -> [u8; METIS_PACKET_LEN] {
    let mut pkt = [0u8; METIS_PACKET_LEN];
    pkt[0] = 0xEF;
    pkt[1] = 0xFE;
    pkt[2] = 0x01;
    pkt[3] = 0x02;
    pkt[4..8].copy_from_slice(&seq.to_be_bytes());
    pkt[METIS_HEADER_LEN..METIS_HEADER_LEN + USB_FRAME_LEN].copy_from_slice(&frame_a);
    pkt[METIS_HEADER_LEN + USB_FRAME_LEN..].copy_from_slice(&frame_b);
    pkt
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
    /// Reused across every `pump_one_packet` call (PR #75 review, round 3)
    /// rather than declared fresh per call: at the supported 8-DDC/192 kHz
    /// ceiling this runs ~9,600 times/sec, so a fresh `[0u8; RECV_BUF_LEN]`
    /// stack array each time zero-initializes ~600 MiB/s merely to receive
    /// ~10 MiB/s of real UDP data -- real memory-bandwidth pressure on the
    /// Pi4 CPU budget this driver has to fit (AGENTS.md). `recv()`
    /// overwrites however many bytes it returns each call and every reader
    /// only ever looks at `recv_buf[..n]`, so stale trailing bytes from a
    /// previous, larger read are never observed -- reuse needs no
    /// re-zeroing between calls.
    recv_buf: [u8; RECV_BUF_LEN],
    /// Shared with every `HpsdrIqSource` handle for this device (a clone
    /// of the same `Arc`, MAN-55) -- flipped `true` the first time a
    /// datagram actually demuxes as valid IQ. A caller checking this
    /// *outside* this mutex-guarded struct (via `HpsdrIqSource`'s own
    /// copy) never needs to lock `Inner` just to poll liveness.
    confirmed_live: Arc<AtomicBool>,
    /// Every C&C setting (ADDR=0x00 general + one per configured
    /// receiver's frequency) this device needs configured, built once at
    /// `open()` time from `HpsdrConfig` (MAN-55 Finding 1). Always has
    /// `1 + ddc_count >= 2` entries (`HpsdrConfig::validate` requires
    /// `ddc_count >= 1`), so the modulo indexing in
    /// `send_next_cc_frames` never divides by a length < 2.
    cc_frames: Vec<[u8; 5]>,
    /// Round-robin position into `cc_frames` for the next keepalive tick's
    /// C&C packet (module docs: real clients cycle settings continuously
    /// rather than sending once).
    cc_cursor: usize,
    /// Outbound C&C packet sequence number (MAN-55) -- incremented on
    /// every C&C send, mirroring piHPSDR's own per-frame sequence counter.
    cc_seq: u32,
}

impl Inner {
    /// Receive and demux Metis packets until one yields real IQ samples,
    /// filling every DDC's queue and updating the gap detector. All
    /// consumer threads share this single socket, so whichever
    /// `HpsdrIqSource::read` finds its own queue empty drives the pump.
    ///
    /// MAN-22: a malformed, truncated, or adversarially-crafted UDP
    /// datagram must not take the input pipeline down. Before this fix, a
    /// length-correct-but-bad-sync-bytes datagram propagated a `bail!` out
    /// of `demux_metis_packet` through this function's `?`, which unwound
    /// all the way out of `manta-engine::listen`'s read loop and killed the
    /// whole process — a single crafted 1032-byte packet was a full DoS.
    /// Both malformed-length and malformed-content packets are now counted
    /// and discarded, and the pump keeps going.
    fn pump_one_packet(&mut self) -> Result<()> {
        let mut consecutive_timeouts = 0u32;
        let mut consecutive_malformed = 0u32;
        loop {
            // Re-checked every iteration (not just once at function entry)
            // so a sustained run of timeouts or malformed packets doesn't
            // starve the HL2 gateware watchdog of the periodic traffic it
            // needs (PR #75 review, round 1) -- `send_keepalive_if_due`
            // itself is a cheap no-op except once per `KEEPALIVE_INTERVAL`.
            self.send_keepalive_if_due()?;

            match self.socket.recv(&mut self.recv_buf) {
                Ok(n) if n == METIS_PACKET_LEN => {
                    let mut demuxed = vec![Vec::new(); self.num_receivers];
                    match demux_metis_packet(&self.recv_buf[..n], self.num_receivers, &mut demuxed)
                    {
                        Ok(()) => {
                            // Cadence is observed only for packets that
                            // pass framing validation (PR #75 review,
                            // round 3) -- `GapStats`'s dropped/gap counters
                            // describe the arrival cadence of valid Metis
                            // packets specifically. Observing here on every
                            // length-correct arrival, malformed or not,
                            // would let malformed traffic during a real
                            // device gap mask that gap once valid IQ
                            // resumes, and could itself spuriously
                            // increment the dropped/gap counters.
                            self.gap.observe(Instant::now());
                            // MAN-55: the first VALID Metis packet is the
                            // only real evidence a live device is on the
                            // other end -- UdpSocket::connect()/the
                            // initial start-command send() both succeed
                            // with no peer response required at all, so
                            // "HpsdrDevice::open didn't error" proves
                            // nothing about actual liveness. store()'d
                            // unconditionally on every success rather than
                            // only the first (Relaxed, no ordering
                            // requirement with anything else -- a plain
                            // liveness flag) -- redundant after the first
                            // time, cheap enough not to bother special-
                            // casing.
                            self.confirmed_live.store(true, Ordering::Relaxed);
                            for (queue, samples) in self.queues.iter_mut().zip(demuxed) {
                                queue.extend(samples);
                            }
                            return Ok(());
                        }
                        Err(_) => {
                            // Right length, but failed sync/framing
                            // validation (e.g. adversarial or corrupt
                            // payload). Discard and keep pumping rather
                            // than propagating a fatal error for one bad
                            // packet.
                            self.gap.record_malformed();
                            consecutive_malformed += 1;
                        }
                    }
                }
                Ok(_) => {
                    // Length mismatch -- too short, or too long (now always
                    // reported as a length mismatch rather than truncated
                    // or WSAEMSGSIZE'd, per RECV_BUF_LEN's doc comment).
                    self.gap.record_malformed();
                    consecutive_malformed += 1;
                }
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

            if consecutive_malformed >= MAX_CONSECUTIVE_MALFORMED {
                bail!(
                    "HPSDR device stalled: {} consecutive malformed datagrams with no valid \
                     Metis packet -- corrupt hardware or a spoofed/non-Metis stream",
                    MAX_CONSECUTIVE_MALFORMED
                );
            }
        }
    }

    fn send_keepalive_if_due(&mut self) -> Result<()> {
        if self.last_keepalive.elapsed() >= KEEPALIVE_INTERVAL {
            self.socket
                .send(&build_start_command())
                .context("send HPSDR keepalive")?;
            self.send_next_cc_frames()?;
            self.last_keepalive = Instant::now();
        }
        Ok(())
    }

    /// Send the next pair of C&C settings frames in `cc_frames`' rotation
    /// (MAN-55 Finding 1): general settings (sample rate, receiver count)
    /// and each configured receiver's tuned frequency, cycled continuously
    /// on the keepalive cadence so the device's actual configuration
    /// converges even across UDP loss, rather than relying on a single
    /// fire-and-forget packet at connect time (module docs, "Sending
    /// cadence" section of the MAN-55 decision doc).
    fn send_next_cc_frames(&mut self) -> Result<()> {
        let n = self.cc_frames.len();
        let a = self.cc_frames[self.cc_cursor % n];
        let b = self.cc_frames[(self.cc_cursor + 1) % n];
        let pkt = build_cc_packet(self.cc_seq, build_cc_usb_frame(a), build_cc_usb_frame(b));
        self.socket
            .send(&pkt)
            .context("send HPSDR C&C tuning packet")?;
        self.cc_seq = self.cc_seq.wrapping_add(1);
        self.cc_cursor = (self.cc_cursor + 2) % n;
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

        // MAN-55 Finding 1: build every C&C setting this device needs
        // configured once, up front -- general settings plus one
        // frequency entry per DDC. `cfg.validate()` above already
        // guarantees these can't fail (encodable sample rate, ddc_count
        // in 1..=12), so a failure here would be a real bug, not a
        // reachable runtime condition.
        let mut cc_frames = Vec::with_capacity(1 + cfg.ddc_count);
        cc_frames.push(
            build_general_cc(cfg.sample_rate_hz, cfg.ddc_count)
                .context("build HPSDR general C&C frame")?,
        );
        for (rx_index, &freq_hz) in cfg.center_freq_hz.iter().enumerate() {
            cc_frames.push(
                build_rx_freq_cc(rx_index, freq_hz)
                    .with_context(|| format!("build HPSDR RX{} C&C frame", rx_index + 1))?,
            );
        }

        let confirmed_live = Arc::new(AtomicBool::new(false));

        let inner = Arc::new(Mutex::new(Inner {
            socket,
            queues: vec![VecDeque::new(); cfg.ddc_count],
            gap: GapDetector::new(cfg.sample_rate_hz, cfg.ddc_count, 1.5),
            num_receivers: cfg.ddc_count,
            last_keepalive: Instant::now(),
            recv_buf: [0u8; RECV_BUF_LEN],
            confirmed_live: confirmed_live.clone(),
            cc_frames,
            cc_cursor: 0,
            cc_seq: 0,
        }));

        Ok((0..cfg.ddc_count)
            .map(|ddc_index| HpsdrIqSource {
                inner: inner.clone(),
                ddc_index,
                sample_rate_hz: cfg.sample_rate_hz,
                center_freq_hz: cfg.center_freq_hz[ddc_index],
                confirmed_live: confirmed_live.clone(),
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
    /// Same underlying flag as `Inner::confirmed_live` (a clone of the
    /// same `Arc`, MAN-55) -- kept here too so `confirmed_live_handle()`
    /// can hand it out without locking `inner`'s mutex just to read a
    /// liveness bit.
    confirmed_live: Arc<AtomicBool>,
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

    fn confirmed_live_handle(&self) -> Option<Arc<AtomicBool>> {
        Some(self.confirmed_live.clone())
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
    fn cc_sample_rate_code_matches_protocol_encoding() {
        assert_eq!(cc_sample_rate_code(48_000.0).unwrap(), 0b00);
        assert_eq!(cc_sample_rate_code(96_000.0).unwrap(), 0b01);
        assert_eq!(cc_sample_rate_code(192_000.0).unwrap(), 0b10);
        assert_eq!(cc_sample_rate_code(384_000.0).unwrap(), 0b11);
        assert!(cc_sample_rate_code(44_100.0).is_err());
    }

    /// PR #79 review, round 1, P2: a near-miss rate must be rejected, not
    /// silently treated as the nearest canonical rate -- the prior
    /// tolerance-based match accepted values like 191999.75 Hz as 192 kHz
    /// while `sample_rate()` kept reporting the original, subtly-wrong
    /// value.
    #[test]
    fn cc_sample_rate_code_rejects_a_near_miss_rate() {
        assert!(cc_sample_rate_code(191_999.75).is_err());
        assert!(cc_sample_rate_code(192_000.25).is_err());
    }

    #[test]
    fn cc_rx_freq_addr_matches_protocol_gap_at_0x09_through_0x11() {
        assert_eq!(cc_rx_freq_addr(0).unwrap(), 0x02);
        assert_eq!(cc_rx_freq_addr(6).unwrap(), 0x08);
        assert_eq!(cc_rx_freq_addr(7).unwrap(), 0x12);
        assert_eq!(cc_rx_freq_addr(11).unwrap(), 0x16);
        assert!(cc_rx_freq_addr(12).is_err());
    }

    #[test]
    fn build_general_cc_encodes_rate_and_receiver_count() {
        let cc = build_general_cc(192_000.0, 3).unwrap();
        assert_eq!(cc[0], cc_c0(0x00));
        assert_eq!(cc[1] & 0x03, 0b10);
        assert_eq!((cc[4] >> 3) & 0x0F, 2); // 3 receivers - 1
    }

    #[test]
    fn build_rx_freq_cc_encodes_big_endian_hz_no_scaling() {
        let cc = build_rx_freq_cc(0, 14_200_000.0).unwrap();
        assert_eq!(cc[0], cc_c0(0x02));
        let freq = u32::from_be_bytes([cc[1], cc[2], cc[3], cc[4]]);
        assert_eq!(freq, 14_200_000);

        // RX8 resumes at 0x12, skipping the 0x09-0x11 TX/Alex/CW gap.
        let cc8 = build_rx_freq_cc(7, 21_050_000.0).unwrap();
        assert_eq!(cc8[0], cc_c0(0x12));
    }

    /// PR #79 review, round 1, P2: a frequency outside the wire's u32 Hz
    /// range must be rejected, not silently clamped -- the prior
    /// `.clamp(0.0, u32::MAX as f64)` saturated a too-large value to
    /// `u32::MAX` while `HpsdrIqSource::center_freq_hz()` kept reporting
    /// the original, unclamped value.
    #[test]
    fn build_rx_freq_cc_rejects_out_of_range_frequencies() {
        assert!(build_rx_freq_cc(0, u32::MAX as f64 + 1.0).is_err());
        assert!(build_rx_freq_cc(0, -1.0).is_err());
        assert!(build_rx_freq_cc(0, f64::NAN).is_err());
        assert!(build_rx_freq_cc(0, f64::INFINITY).is_err());
    }

    #[test]
    fn build_cc_packet_uses_the_real_usb_data_endpoint_2_header() {
        // PR #79 review, round 1, P1: every real Protocol 1 host->device
        // USB data packet starts `EF FE 01 02` followed by the 4-byte
        // sequence number -- not `0x02` directly in byte 2 with the
        // sequence shifted into bytes 3-6 (the original, hardware-
        // unrecognizable encoding).
        let frame = [0u8; USB_FRAME_LEN];
        let pkt = build_cc_packet(0x1234_5678, frame, frame);
        assert_eq!(&pkt[0..4], &[0xEF, 0xFE, 0x01, 0x02]);
        assert_eq!(&pkt[4..8], &0x1234_5678u32.to_be_bytes());
    }

    #[test]
    fn hpsdr_config_validate_rejects_unencodable_sample_rate() {
        let cfg = HpsdrConfig {
            host: "127.0.0.1".into(),
            port: CONTROL_PORT,
            ddc_count: 1,
            sample_rate_hz: 44_100.0,
            center_freq_hz: vec![14_025_000.0],
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn hpsdr_config_validate_rejects_more_than_12_receivers() {
        let cfg = HpsdrConfig {
            host: "127.0.0.1".into(),
            port: CONTROL_PORT,
            ddc_count: 13,
            sample_rate_hz: 48_000.0,
            center_freq_hz: vec![14_025_000.0; 13],
        };
        assert!(cfg.validate().is_err());
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

    /// PR #79 review, round 1, P2: caught at config-validate time for
    /// every caller (not just `manta-cli`'s own `--hpsdr-freq` parser,
    /// which only rejects non-finite/non-positive values), before any
    /// device is opened.
    #[test]
    fn config_validate_rejects_out_of_range_center_freq() {
        let cfg = HpsdrConfig {
            host: "127.0.0.1".into(),
            port: CONTROL_PORT,
            ddc_count: 1,
            sample_rate_hz: 192_000.0,
            center_freq_hz: vec![u32::MAX as f64 + 1.0],
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

    /// MAN-55: `HpsdrDevice::open` succeeding must not be mistaken for
    /// confirmed liveness -- `UdpSocket::connect`/the initial start-command
    /// `send` both require no peer response at all, so a source pointed at
    /// an address nothing is listening on "opens" successfully with zero
    /// evidence anything real is there. `confirmed_live_handle()` must
    /// start false and flip true only once a real Metis packet actually
    /// demuxes -- both DDCs on the same device share one handle (the
    /// underlying flag lives on the shared `Inner`, module docs), so
    /// observing it via a DDC that never itself calls `read()` still
    /// proves the shared state updated correctly, not just the reading
    /// DDC's own local view.
    #[test]
    fn confirmed_live_handle_starts_false_and_flips_true_on_first_valid_packet() {
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

        let live0 = sources[0].confirmed_live_handle().expect(
            "HpsdrIqSource must report Some(handle) -- open() alone never confirms liveness",
        );
        let live1 = sources[1]
            .confirmed_live_handle()
            .expect("every DDC on the same device shares the same liveness flag");
        assert!(
            !live0.load(Ordering::Relaxed),
            "must start false immediately after open() -- nothing has responded yet"
        );
        assert!(!live1.load(Ordering::Relaxed));

        let mut discard = [0u8; METIS_PACKET_LEN];
        let (_, client_addr) = device_socket.recv_from(&mut discard).unwrap();

        // Still false -- only a genuinely valid Metis packet counts.
        device_socket
            .send_to(&[0xFFu8; METIS_PACKET_LEN], client_addr)
            .unwrap();
        let mut buf = vec![Complex32::new(0.0, 0.0); 4096];
        // DDC 0's read() drives the shared pump; the malformed packet
        // above is silently discarded (MAN-22) so this call blocks until
        // a real one arrives below -- send it from a second thread so
        // this call doesn't deadlock waiting on a packet the test hasn't
        // sent yet.
        let sender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            let pkt = synth_metis_packet(2, |_| Complex32::new(0.1, 0.1));
            device_socket.send_to(&pkt, client_addr).unwrap();
        });
        let n = sources[0].read(&mut buf).unwrap();
        sender.join().unwrap();
        assert!(n > 0);

        assert!(
            live0.load(Ordering::Relaxed),
            "must flip true once a valid packet is actually processed"
        );
        assert!(
            live1.load(Ordering::Relaxed),
            "DDC 1's handle must observe the same flip -- it's the same shared flag, \
             even though DDC 1 itself never called read()"
        );
    }

    /// MAN-22 regression: a malformed, truncated, or adversarially-crafted
    /// UDP datagram must not crash or wedge the input pipeline, and the
    /// rejection must be counted rather than silent. Sends a mix of
    /// too-short, too-long, and correct-length-but-bad-sync-bytes datagrams
    /// (the exact shape that used to `bail!` out of `demux_metis_packet`
    /// and kill the whole read via `?` propagation) at a live loopback
    /// `HpsdrDevice`, then confirms the source is still alive and correctly
    /// reads a subsequent genuine packet.
    #[test]
    fn malformed_udp_packets_are_discarded_and_counted_not_fatal() {
        let device_socket = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        let device_addr = device_socket.local_addr().unwrap();
        device_socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        let cfg = HpsdrConfig {
            host: device_addr.ip().to_string(),
            port: device_addr.port(),
            ddc_count: 1,
            sample_rate_hz: 192_000.0,
            center_freq_hz: vec![14_025_000.0],
        };
        let mut sources = HpsdrDevice::open(cfg).unwrap();
        assert_eq!(sources.len(), 1);

        // Consume the start command so it isn't mistaken for a datagram.
        let mut discard = [0u8; METIS_PACKET_LEN];
        let (_, client_addr) = device_socket.recv_from(&mut discard).unwrap();

        // 1. Too short.
        device_socket.send_to(&[0u8; 10], client_addr).unwrap();
        // 2. Too long.
        device_socket
            .send_to(&[0u8; METIS_PACKET_LEN + 50], client_addr)
            .unwrap();
        // 3. Correct length, but adversarial garbage (all 0xFF) -- fails
        //    the USB sync-byte check inside demux_metis_packet, which used
        //    to be fatal via `?`.
        device_socket
            .send_to(&[0xFFu8; METIS_PACKET_LEN], client_addr)
            .unwrap();
        // 4. Correct length, all-zero -- also fails the sync-byte check.
        device_socket
            .send_to(&[0u8; METIS_PACKET_LEN], client_addr)
            .unwrap();

        // Now a genuine packet -- the pipeline must still be able to
        // produce real samples after four malformed/adversarial datagrams.
        let value = Complex32::new(0.42, -0.17);
        let pkt = synth_metis_packet(1, |_| value);
        device_socket.send_to(&pkt, client_addr).unwrap();

        let mut buf = vec![Complex32::new(0.0, 0.0); 4096];
        let n = sources[0]
            .read(&mut buf)
            .expect("read must succeed despite preceding malformed packets");
        assert!(n > 0, "expected real samples after malformed packets");
        assert!((buf[0].re - value.re).abs() < 1e-4);
        assert!((buf[0].im - value.im).abs() < 1e-4);

        let stats = sources[0].gap_stats();
        assert_eq!(
            stats.malformed_packets, 4,
            "all four malformed/adversarial datagrams must be counted, not silently dropped"
        );
    }

    /// PR #75 review, round 1, finding 1: a receive buffer sized exactly to
    /// `METIS_PACKET_LEN` can't tell an oversized datagram apart from a
    /// truncated read of it. Craft a datagram whose FIRST `METIS_PACKET_LEN`
    /// bytes are a perfectly valid Metis packet, with extra garbage bytes
    /// appended past that boundary. With the old exactly-sized buffer, this
    /// would truncate on read and be silently ACCEPTED as valid IQ -- the
    /// trailing tampering invisible. It must now be rejected outright as a
    /// length mismatch instead.
    #[test]
    fn an_oversized_datagram_with_a_valid_prefix_is_rejected_not_silently_truncated() {
        let device_socket = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        let device_addr = device_socket.local_addr().unwrap();
        device_socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        let cfg = HpsdrConfig {
            host: device_addr.ip().to_string(),
            port: device_addr.port(),
            ddc_count: 1,
            sample_rate_hz: 192_000.0,
            center_freq_hz: vec![14_025_000.0],
        };
        let mut sources = HpsdrDevice::open(cfg).unwrap();

        let mut discard = [0u8; METIS_PACKET_LEN];
        let (_, client_addr) = device_socket.recv_from(&mut discard).unwrap();

        // A genuinely valid packet, content-wise -- but with 50 extra
        // tampered bytes appended, which a truncating read would silently
        // drop, making this indistinguishable from the clean packet sent
        // right after it.
        let tampered_value = Complex32::new(0.11, 0.22);
        let mut tampered = synth_metis_packet(1, |_| tampered_value);
        tampered.extend_from_slice(&[0xAAu8; 50]);
        assert_eq!(tampered.len(), METIS_PACKET_LEN + 50);
        device_socket.send_to(&tampered, client_addr).unwrap();

        let clean_value = Complex32::new(-0.33, 0.44);
        let clean_pkt = synth_metis_packet(1, |_| clean_value);
        device_socket.send_to(&clean_pkt, client_addr).unwrap();

        let mut buf = vec![Complex32::new(0.0, 0.0); 4096];
        let n = sources[0]
            .read(&mut buf)
            .expect("read must succeed via the clean packet");
        assert!(n > 0);
        assert!(
            (buf[0].re - clean_value.re).abs() < 1e-4,
            "must have skipped the tampered/oversized packet's samples entirely, got {:?} \
             (would equal the tampered packet's {:?} if it were silently truncated and accepted)",
            buf[0],
            tampered_value
        );

        assert_eq!(
            sources[0].gap_stats().malformed_packets,
            1,
            "the oversized tampered packet must be counted as malformed, not accepted"
        );
    }

    /// PR #75 review, round 1, finding 2: a continuous flood of
    /// length-correct-but-bad-sync-bytes packets must not loop forever --
    /// `read()` (and therefore `pump_one_packet`) must eventually give up
    /// with a clear error, the same way a silent/stalled device already
    /// does via `MAX_CONSECUTIVE_TIMEOUTS`.
    #[test]
    fn a_sustained_flood_of_malformed_packets_eventually_gives_up_cleanly() {
        let device_socket = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        let device_addr = device_socket.local_addr().unwrap();
        device_socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();

        let cfg = HpsdrConfig {
            host: device_addr.ip().to_string(),
            port: device_addr.port(),
            ddc_count: 1,
            sample_rate_hz: 192_000.0,
            center_freq_hz: vec![14_025_000.0],
        };
        let mut sources = HpsdrDevice::open(cfg).unwrap();

        let mut discard = [0u8; METIS_PACKET_LEN];
        let (_, client_addr) = device_socket.recv_from(&mut discard).unwrap();

        // A background sender keeps the socket continuously fed rather
        // than one giant up-front burst, which would overflow the OS's UDP
        // receive buffer long before `read()` starts draining it and make
        // the exact malformed count below unreliable. Never sends a single
        // valid packet -- purely a malformed flood.
        let stop = Arc::new(AtomicBool::new(false));
        let stop_sender = stop.clone();
        let sender_socket = device_socket.try_clone().unwrap();
        let sender = std::thread::spawn(move || {
            let garbage = [0xFFu8; METIS_PACKET_LEN];
            while !stop_sender.load(Ordering::Relaxed) {
                let _ = sender_socket.send_to(&garbage, client_addr);
            }
        });

        let mut buf = vec![Complex32::new(0.0, 0.0); 4096];
        let err = sources[0].read(&mut buf).expect_err(
            "a sustained malformed flood must eventually surface as a clear error, not hang",
        );

        stop.store(true, Ordering::Relaxed);
        sender.join().unwrap();

        assert!(
            err.to_string().contains("malformed"),
            "expected a malformed-flood stall error, got: {err}"
        );
        assert_eq!(
            sources[0].gap_stats().malformed_packets,
            MAX_CONSECUTIVE_MALFORMED as u64,
            "must have counted exactly the bound's worth of malformed packets before giving up"
        );
    }

    /// MAN-55 Finding 1: `send_keepalive_if_due` must actually transmit
    /// C&C tuning packets (endpoint `0x02`) on its 1s cadence, not just
    /// the start/stop keepalive (endpoint `0x04`) -- and the first such
    /// packet's two USB frames must carry the general settings (ADDR
    /// 0x00: rate + receiver count) and RX1's tuned frequency (ADDR
    /// 0x02), matching `HpsdrConfig`. No explicit sleep: the background
    /// thread just blocks on `recv_from` (inheriting `device_socket`'s
    /// read timeout via `try_clone`) until `read()`'s internal pump loop
    /// crosses `KEEPALIVE_INTERVAL` on its own and fires the C&C send.
    #[test]
    fn keepalive_sends_cc_tuning_packet_with_general_and_rx_frequency_frames() {
        let device_socket = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        let device_addr = device_socket.local_addr().unwrap();
        device_socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();

        let cfg = HpsdrConfig {
            host: device_addr.ip().to_string(),
            port: device_addr.port(),
            ddc_count: 2,
            sample_rate_hz: 192_000.0,
            center_freq_hz: vec![14_025_000.0, 14_030_000.0],
        };
        let mut sources = HpsdrDevice::open(cfg).unwrap();

        // Drain the initial start command sent by open().
        let mut discard = [0u8; METIS_PACKET_LEN];
        device_socket.recv_from(&mut discard).unwrap();

        let sender_socket = device_socket.try_clone().unwrap();
        let sender = std::thread::spawn(move || loop {
            let mut buf = [0u8; METIS_PACKET_LEN];
            let (n, from) = sender_socket
                .recv_from(&mut buf)
                .expect("expected a C&C packet within the keepalive window");
            if n == METIS_PACKET_LEN && buf[2] == 0x01 && buf[3] == 0x02 {
                let pkt = synth_metis_packet(2, |_| Complex32::new(0.1, 0.1));
                sender_socket.send_to(&pkt, from).unwrap();
                return buf;
            }
            // n == METIS_PACKET_LEN && buf[2] == 0x04: the start/stop
            // keepalive re-send -- keep waiting for the C&C packet.
        });

        let mut buf = vec![Complex32::new(0.0, 0.0); 4096];
        let n = sources[0].read(&mut buf).unwrap();
        assert!(n > 0);
        let cc_pkt = sender.join().unwrap();

        assert_eq!(
            cc_pkt[2], 0x01,
            "C&C packets are Metis USB-data-packet type 0x01"
        );
        assert_eq!(
            cc_pkt[3], 0x02,
            "C&C packets target endpoint 2 (host -> device)"
        );

        let frame0 = &cc_pkt[METIS_HEADER_LEN..METIS_HEADER_LEN + USB_FRAME_LEN];
        assert_eq!(&frame0[0..3], &USB_SYNC);
        assert_eq!(
            frame0[3],
            cc_c0(0x00),
            "first frame is ADDR=0x00 general settings"
        );
        assert_eq!(frame0[4] & 0x03, 0b10, "192 kHz sample rate code");
        assert_eq!((frame0[7] >> 3) & 0x0F, 1, "2 receivers encoded as (2-1)");

        let frame1 = &cc_pkt[METIS_HEADER_LEN + USB_FRAME_LEN..];
        assert_eq!(&frame1[0..3], &USB_SYNC);
        assert_eq!(
            frame1[3],
            cc_c0(0x02),
            "second frame is ADDR=0x02, RX1 frequency"
        );
        let freq = u32::from_be_bytes([frame1[4], frame1[5], frame1[6], frame1[7]]);
        assert_eq!(freq, 14_025_000);
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
