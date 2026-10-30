//! openHPSDR protocol 2 wire format (MAN-305): packet builders, the DDC IQ
//! datagram parser, the board table, the Alex filter words and the
//! sequence-number tracker. Pure code with no sockets, after the
//! `ka9q/wire.rs` precedent.
//!
//! Every offset and value is written from the tables in
//! `docs/DECISIONS/2026-10-19-man305-hpsdr-protocol-2-input.md` (D6, D7,
//! D9, D10), which record facts read from piHPSDR's `new_protocol.c` and
//! `alex.h`; no code was translated from those GPL sources (MAN-140 D1).

use num_complex::Complex32;

/// Length of a general packet (host → `port`), in bytes (D6).
pub const GENERAL_LEN: usize = 60;
/// Length of an rx-specific packet (host → `port + 1`), in bytes (D6).
pub const RX_SPECIFIC_LEN: usize = 1444;
/// Length of a tx-specific packet (host → `port + 2`), in bytes (D6).
pub const TX_SPECIFIC_LEN: usize = 60;
/// Length of a high-priority packet (host → `port + 3`), in bytes (D6).
pub const HIGH_PRIORITY_LEN: usize = 1444;
/// Length of one DDC IQ datagram (radio `port + 11 + n` → host), in bytes (D9).
pub const IQ_DATAGRAM_LEN: usize = 1444;
/// IQ pairs carried by one DDC IQ datagram at 24 bits per sample (D9).
pub const SAMPLES_PER_DATAGRAM: usize = 238;
/// Bits per sample the IQ datagram must declare, and the rx-specific
/// packet requests (D6, D9).
pub const BITS_PER_SAMPLE: u16 = 24;
/// 2^23: the divisor every reference client scales protocol-2 24-bit IQ
/// samples by (D9). Protocol 1 keeps its own constant.
pub const P2_IQ_FULL_SCALE: f32 = 8_388_608.0;
/// The radio's ADC clock, in Hz: phase words are `f * 2^32 / clock` (D6).
pub const ADC_CLOCK_HZ: f64 = 122_880_000.0;

/// Every packet's per-port sequence number: BE u32 at bytes 0..4, counting
/// from 0 (D6 preamble; D9 `[0..4]`).
const SEQ_OFFSET: usize = 0;

/// General `[4]=0x00`: the command byte (D6 general row).
const GENERAL_COMMAND: usize = 4;
/// General `[37]=0x08`: frequencies are sent as phase words (D6 general row).
const GENERAL_FREQ_MODE: usize = 37;
/// The general `[37]` value: phase words, not Hz (D6 general row).
const GENERAL_FREQ_MODE_PHASE_WORDS: u8 = 0x08;
/// General `[38]=0x01`: the radio's watchdog on (D6 general row).
const GENERAL_WATCHDOG: usize = 38;
/// The general `[38]` value: watchdog on (D6 general row).
const GENERAL_WATCHDOG_ON: u8 = 0x01;
/// General `[58]=0x00`: PA off (D6 general row).
const GENERAL_PA: usize = 58;
/// General `[59]`: the filter-board enable from the D7 table (D6 general row).
const GENERAL_FILTER_ENABLE: usize = 59;

/// rx-specific `[4]`: the ADC count from the D7 table (D6 rx-specific row).
const RX_ADC_COUNT: usize = 4;
/// rx-specific `[5]=0x00`: dither off (D6 rx-specific row).
const RX_DITHER: usize = 5;
/// rx-specific `[6]=0x00`: random off (D6 rx-specific row).
const RX_RANDOM: usize = 6;
/// rx-specific `[7]`: the DDC enable mask (D6 rx-specific row).
const RX_DDC_ENABLE: usize = 7;
/// The rx-specific `[7]` value: DDC0 only (D6 rx-specific row).
const RX_DDC_ENABLE_DDC0: u8 = 0x01;
/// rx-specific `[17]=0`: DDC0 takes ADC0 (D6 rx-specific row).
const RX_DDC0_ADC: usize = 17;
/// rx-specific `[18..20]`: DDC0's rate in kHz, BE u16 (D6 rx-specific row).
const RX_DDC0_RATE: usize = 18;
/// rx-specific `[22]=24`: DDC0's bits per sample (D6 rx-specific row).
const RX_DDC0_BITS: usize = 22;

/// tx-specific `[4]=1`: one DAC (D6 tx-specific row).
const TX_DAC_COUNT: usize = 4;
/// The tx-specific `[4]` value: one DAC (D6 tx-specific row).
const TX_ONE_DAC: u8 = 1;

/// High-priority `[4]`: bit 0 is run; PTT (bit 1) is never set (D6
/// high-priority row).
const HP_RUN: usize = 4;
/// The high-priority `[4]` run bit (D6 high-priority row).
const HP_RUN_BIT: u8 = 0x01;
/// High-priority `[9..13]`: DDC0 phase word, BE u32 (D6 high-priority row).
const HP_DDC0_PHASE: usize = 9;
/// High-priority `[329..333]`: the DUC phase word, the same as DDC0's (D6
/// high-priority row).
const HP_DUC_PHASE: usize = 329;
/// High-priority `[345]=0`: drive level (D6 high-priority row).
const HP_DRIVE: usize = 345;
/// High-priority `[1428..1432]`: alex1, BE u32 (D6 high-priority row).
const HP_ALEX1: usize = 1428;
/// High-priority `[1432..1436]`: alex0, BE u32 (D6 high-priority row).
const HP_ALEX0: usize = 1432;
/// High-priority `[1443]`: ADC0's step attenuator, 0..=31 dB in 1 dB units;
/// `[1442]` is ADC1's and stays 0 (MAN-308 D2; MAN-305 D6 set both to 0).
const HP_STEP_ATTENUATOR_ADC0: usize = 1443;
/// High-priority `[1442]`: ADC1's step attenuator, always 0 dB (MAN-308 D2).
const HP_STEP_ATTENUATOR_ADC1: usize = 1442;
/// The step attenuator's range, dB (MAN-308 D2).
pub const MAX_STEP_ATTENUATION_DB: u8 = 31;

/// Length of a high-priority status datagram (radio `port + 1` → host).
pub const STATUS_LEN: usize = 60;
/// Status `[5]`: ADC overload bits, bit i = ADC i, 1 = overload (MAN-308 D2).
const STATUS_ADC_OVERLOAD: usize = 5;
/// ADC0's overload bit in status `[5]` (MAN-308 D2).
pub const ADC0_OVERLOAD: u8 = 0x01;

/// IQ datagram `[12..14]`: bits per sample, BE u16 (D9).
const IQ_BITS: usize = 12;
/// IQ datagram `[14..16]`: samples per frame, BE u16 (D9).
const IQ_SAMPLES: usize = 14;
/// IQ datagram `[16..1444]`: the IQ pairs (D9).
const IQ_PAYLOAD: usize = 16;
/// Bytes per 24-bit sample component (D9).
const IQ_COMPONENT_BYTES: usize = 3;
/// Bytes per IQ pair: I then Q (D9).
const IQ_PAIR_BYTES: usize = 2 * IQ_COMPONENT_BYTES;

/// The four rates the rx-specific rate field encodes, in kHz (D14).
const ENCODABLE_RATES_KHZ: [u16; 4] = [48, 96, 192, 384];

/// `ALEX_BYPASS_HPF`, equal to `ALEX_ANAN7000_RX_BYPASS_BPF`: the same bit
/// bypasses the HPF bank on Classic boards and the BPF bank on band-pass
/// boards (D7).
const ALEX_BYPASS: u32 = 0x1000;
/// HPF 1.5 MHz (D7).
const HPF_1_5_MHZ: u32 = 0x40;
/// HPF 6.5 MHz (D7).
const HPF_6_5_MHZ: u32 = 0x20;
/// HPF 9.5 MHz (D7).
const HPF_9_5_MHZ: u32 = 0x10;
/// HPF 13 MHz (D7).
const HPF_13_MHZ: u32 = 0x02;
/// HPF 20 MHz (D7).
const HPF_20_MHZ: u32 = 0x04;
/// HPF bank's 6 m preamp (D7).
const HPF_6M_PREAMP: u32 = 0x08;
/// LPF 160 m (D7).
const LPF_160M: u32 = 0x0080_0000;
/// LPF 80 m (D7).
const LPF_80M: u32 = 0x0040_0000;
/// LPF 60/40 m (D7).
const LPF_60_40M: u32 = 0x0020_0000;
/// LPF 30/20 m (D7).
const LPF_30_20M: u32 = 0x0010_0000;
/// LPF 17/15 m (D7).
const LPF_17_15M: u32 = 0x8000_0000;
/// LPF 12/10 m (D7).
const LPF_12_10M: u32 = 0x4000_0000;
/// LPF 6 m, also the LPF bypass (D7).
const LPF_6M_BYPASS: u32 = 0x2000_0000;
/// BPF 160 m (D7).
const BPF_160M: u32 = 0x40;
/// BPF 80/60 m (D7).
const BPF_80_60M: u32 = 0x20;
/// BPF 40/30 m (D7).
const BPF_40_30M: u32 = 0x10;
/// BPF 20/15 m (D7).
const BPF_20_15M: u32 = 0x02;
/// BPF 12/10 m (D7).
const BPF_12_10M: u32 = 0x04;
/// BPF 6 m (D7).
const BPF_6M: u32 = 0x08;

/// HPF(f) (D7 ladder): the first row whose limit `f` is below, else the 6 m
/// preamp.
const HPF_LADDER: [(f64, u32); 6] = [
    (1_800_000.0, ALEX_BYPASS),
    (6_500_000.0, HPF_1_5_MHZ),
    (9_500_000.0, HPF_6_5_MHZ),
    (13_000_000.0, HPF_9_5_MHZ),
    (20_000_000.0, HPF_13_MHZ),
    (50_000_000.0, HPF_20_MHZ),
];
/// LPF(f) (D7 ladder): the first row whose limit `f` is above, else 160 m.
const LPF_LADDER: [(f64, u32); 6] = [
    (35_600_000.0, LPF_6M_BYPASS),
    (24_000_000.0, LPF_12_10M),
    (16_500_000.0, LPF_17_15M),
    (8_000_000.0, LPF_30_20M),
    (5_000_000.0, LPF_60_40M),
    (2_500_000.0, LPF_80M),
];
/// BPF(f) (D7 ladder): the first row whose limit `f` is below, else 6 m.
const BPF_LADDER: [(f64, u32); 6] = [
    (1_500_000.0, ALEX_BYPASS),
    (2_100_000.0, BPF_160M),
    (5_500_000.0, BPF_80_60M),
    (11_000_000.0, BPF_40_30M),
    (22_000_000.0, BPF_20_15M),
    (35_000_000.0, BPF_12_10M),
];

/// The unknown-board alex0: LPF bypass | HPF/BPF bypass (D7 last row).
const BYPASS_ALEX0: u32 = LPF_6M_BYPASS | ALEX_BYPASS;
/// The unknown-board alex1: LPF bypass (D7 last row).
const BYPASS_ALEX1: u32 = LPF_6M_BYPASS;

/// A stale (late or duplicate) datagram is at most this many behind
/// `expected`; further back is a restart (D10).
const MAX_STALE_BEHIND: i32 = 64;
/// This many stale datagrams in a row are a restart (D10).
const STALE_RUN_RESTART: u32 = 8;

/// Which family of Alex filter words a board takes (D7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterScheme {
    /// No filter board: both words are 0 (Atlas, Hermes Lite).
    None,
    /// Pre-Orion-II boards: alex0 = HPF(f) | LPF(f), alex1 = LPF(f).
    Classic,
    /// Orion II and Saturn: alex0 = BPF(f) | LPF(f), alex1 = bypass | LPF(f).
    BandPassDual,
    /// G2E: alex0 = BPF(f) | LPF(f), alex1 = LPF(f).
    BandPass,
    /// Unknown board or discovery skipped: the bypass words.
    Bypass,
}

/// What the bring-up needs to know about the radio's board (D7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoardInfo {
    /// Discovery byte 11, or `None` when discovery was skipped.
    pub id: Option<u8>,
    /// The board's name, or `None` for a board manta does not know.
    pub name: Option<&'static str>,
    /// rx-specific byte 4.
    pub adc_count: u8,
    /// General byte 59.
    pub filter_enable: u8,
    pub scheme: FilterScheme,
}

/// The D7 board table: `None` (discovery skipped) and unknown numbers both
/// give the unknown-board row.
pub fn board_info(id: Option<u8>) -> BoardInfo {
    let (name, adc_count, filter_enable, scheme) = match id {
        Some(0) => (Some("Atlas"), 1, 0x00, FilterScheme::None),
        Some(1) => (Some("Hermes"), 1, 0x01, FilterScheme::Classic),
        Some(2) => (Some("Hermes II"), 1, 0x01, FilterScheme::Classic),
        Some(3) => (Some("Angelia"), 2, 0x01, FilterScheme::Classic),
        Some(4) => (Some("Orion"), 2, 0x01, FilterScheme::Classic),
        Some(5) => (Some("Orion II"), 2, 0x03, FilterScheme::BandPassDual),
        Some(6) => (Some("Hermes Lite"), 1, 0x00, FilterScheme::None),
        Some(10 | 11) => (Some("Saturn"), 2, 0x03, FilterScheme::BandPassDual),
        Some(20) => (Some("G2E"), 1, 0x01, FilterScheme::BandPass),
        _ => (None, 1, 0x01, FilterScheme::Bypass),
    };
    BoardInfo {
        id,
        name,
        adc_count,
        filter_enable,
        scheme,
    }
}

/// `(alex0, alex1)` for `scheme` at DDC0 frequency `freq_hz` (D7).
pub fn alex_words(scheme: FilterScheme, freq_hz: f64) -> (u32, u32) {
    match scheme {
        FilterScheme::None => (0, 0),
        FilterScheme::Classic => (hpf(freq_hz) | lpf(freq_hz), lpf(freq_hz)),
        FilterScheme::BandPassDual => (bpf(freq_hz) | lpf(freq_hz), ALEX_BYPASS | lpf(freq_hz)),
        FilterScheme::BandPass => (bpf(freq_hz) | lpf(freq_hz), lpf(freq_hz)),
        FilterScheme::Bypass => (BYPASS_ALEX0, BYPASS_ALEX1),
    }
}

/// The D7 HPF ladder: "< limit" rows, top down.
fn hpf(freq_hz: f64) -> u32 {
    HPF_LADDER
        .iter()
        .find(|&&(limit, _)| freq_hz < limit)
        .map_or(HPF_6M_PREAMP, |&(_, bits)| bits)
}

/// The D7 LPF ladder: "> limit" rows, top down.
fn lpf(freq_hz: f64) -> u32 {
    LPF_LADDER
        .iter()
        .find(|&&(limit, _)| freq_hz > limit)
        .map_or(LPF_160M, |&(_, bits)| bits)
}

/// The D7 BPF ladder: "< limit" rows, top down.
fn bpf(freq_hz: f64) -> u32 {
    BPF_LADDER
        .iter()
        .find(|&&(limit, _)| freq_hz < limit)
        .map_or(BPF_6M, |&(_, bits)| bits)
}

/// DDC phase word `floor(f * 2^32 / 122.88 MHz)` (D6).
pub fn phase_word(freq_hz: f64) -> u32 {
    // f * 2^32 is exact in f64, and for a whole number of Hz the quotient
    // f * 2^16 / 1875 is either an integer or at least 1/1875 from one, so
    // the rounded division never crosses an integer. Out-of-range input
    // saturates (`as` on f64); D14 validation keeps f in 0 < f < 61.44 MHz.
    (freq_hz * 4_294_967_296.0 / ADC_CLOCK_HZ).floor() as u32
}

/// The rx-specific rate field (kHz) for one of the four encodable rates;
/// `None` for any other rate (D14).
pub fn rate_khz(rate_hz: f64) -> Option<u16> {
    ENCODABLE_RATES_KHZ
        .into_iter()
        .find(|&khz| f64::from(khz) * 1000.0 == rate_hz)
}

/// General packet (D6).
pub fn general_packet(seq: u32, board: &BoardInfo) -> [u8; GENERAL_LEN] {
    let mut p = [0u8; GENERAL_LEN];
    put_u32(&mut p, SEQ_OFFSET, seq);
    p[GENERAL_COMMAND] = 0x00;
    p[GENERAL_FREQ_MODE] = GENERAL_FREQ_MODE_PHASE_WORDS;
    p[GENERAL_WATCHDOG] = GENERAL_WATCHDOG_ON;
    p[GENERAL_PA] = 0x00;
    p[GENERAL_FILTER_ENABLE] = board.filter_enable;
    p
}

/// rx-specific packet for DDC0 only (D6).
pub fn rx_specific_packet(seq: u32, board: &BoardInfo, rate_khz: u16) -> Vec<u8> {
    let mut p = vec![0u8; RX_SPECIFIC_LEN];
    put_u32(&mut p, SEQ_OFFSET, seq);
    p[RX_ADC_COUNT] = board.adc_count;
    p[RX_DITHER] = 0x00;
    p[RX_RANDOM] = 0x00;
    p[RX_DDC_ENABLE] = RX_DDC_ENABLE_DDC0;
    p[RX_DDC0_ADC] = 0;
    p[RX_DDC0_RATE..RX_DDC0_RATE + 2].copy_from_slice(&rate_khz.to_be_bytes());
    // BITS_PER_SAMPLE is 24, which fits the byte.
    p[RX_DDC0_BITS] = BITS_PER_SAMPLE as u8;
    p
}

/// tx-specific packet (D6).
pub fn tx_specific_packet(seq: u32) -> [u8; TX_SPECIFIC_LEN] {
    let mut p = [0u8; TX_SPECIFIC_LEN];
    put_u32(&mut p, SEQ_OFFSET, seq);
    p[TX_DAC_COUNT] = TX_ONE_DAC;
    p
}

/// High-priority packet (D6): `run` sets byte 4 bit 0; the phase word goes
/// to DDC0 and to the DUC; the Alex words to bytes 1428..1436; ADC0's step
/// attenuation to byte 1443, capped at [`MAX_STEP_ATTENUATION_DB`], and 0 dB
/// to ADC1's byte 1442 (MAN-308 D2).
pub fn high_priority_packet(
    seq: u32,
    run: bool,
    phase_word: u32,
    alex0: u32,
    alex1: u32,
    step_attenuation_db: u8,
) -> Vec<u8> {
    let mut p = vec![0u8; HIGH_PRIORITY_LEN];
    put_u32(&mut p, SEQ_OFFSET, seq);
    p[HP_RUN] = if run { HP_RUN_BIT } else { 0 };
    put_u32(&mut p, HP_DDC0_PHASE, phase_word);
    put_u32(&mut p, HP_DUC_PHASE, phase_word);
    p[HP_DRIVE] = 0;
    put_u32(&mut p, HP_ALEX1, alex1);
    put_u32(&mut p, HP_ALEX0, alex0);
    p[HP_STEP_ATTENUATOR_ADC1] = 0;
    p[HP_STEP_ATTENUATOR_ADC0] = step_attenuation_db.min(MAX_STEP_ATTENUATION_DB);
    p
}

/// The ADC overload bits of a high-priority status datagram (radio
/// `port + 1` → host); `None` for any other length (MAN-308 D2).
pub fn adc_overload_bits(buf: &[u8]) -> Option<u8> {
    (buf.len() == STATUS_LEN).then(|| buf[STATUS_ADC_OVERLOAD])
}

/// Write `v` big-endian at `p[at..at + 4]`.
fn put_u32(p: &mut [u8], at: usize, v: u32) {
    p[at..at + 4].copy_from_slice(&v.to_be_bytes());
}

/// Read a big-endian u16 at `p[at..at + 2]`; the caller checked the length.
fn get_u16(p: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([p[at], p[at + 1]])
}

/// One 24-bit big-endian two's-complement component, scaled by 2^-23 (D9).
fn s24(b: &[u8]) -> f32 {
    // Place the 24 bits in the top of an i32, then shift arithmetically
    // so the sign extends.
    (i32::from_be_bytes([b[0], b[1], b[2], 0]) >> 8) as f32 / P2_IQ_FULL_SCALE
}

/// Why a datagram on the IQ port was not a DDC IQ datagram (D9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MalformedIq {
    Length(usize),
    BitsPerSample(u16),
    SamplesPerFrame(u16),
}

/// Parse one DDC IQ datagram, appending its 238 samples to `out`, and
/// return its sequence number. On error `out` is unchanged (D9).
pub fn parse_iq_datagram(buf: &[u8], out: &mut Vec<Complex32>) -> Result<u32, MalformedIq> {
    if buf.len() != IQ_DATAGRAM_LEN {
        return Err(MalformedIq::Length(buf.len()));
    }
    let bits = get_u16(buf, IQ_BITS);
    if bits != BITS_PER_SAMPLE {
        return Err(MalformedIq::BitsPerSample(bits));
    }
    let samples = get_u16(buf, IQ_SAMPLES);
    if usize::from(samples) != SAMPLES_PER_DATAGRAM {
        return Err(MalformedIq::SamplesPerFrame(samples));
    }
    let seq = u32::from_be_bytes([
        buf[SEQ_OFFSET],
        buf[SEQ_OFFSET + 1],
        buf[SEQ_OFFSET + 2],
        buf[SEQ_OFFSET + 3],
    ]);
    // 1444 - 16 = 1428 = 238 pairs of 6 bytes exactly: no remainder.
    let (pairs, _) = buf[IQ_PAYLOAD..].as_chunks::<IQ_PAIR_BYTES>();
    out.reserve(SAMPLES_PER_DATAGRAM);
    out.extend(pairs.iter().map(|pair| {
        let (i, q) = pair.split_at(IQ_COMPONENT_BYTES);
        Complex32::new(s24(i), s24(q))
    }));
    Ok(seq)
}

/// What one sequence number means relative to the stream so far (D10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqEvent {
    /// The first datagram, or the next one in order (including the wrap).
    InOrder,
    /// `n` datagrams were lost immediately before this one.
    Gap(u32),
    /// Late or duplicate: already counted as lost; discard it.
    Stale,
    /// The radio began a new stream; this datagram is the new baseline.
    Restart,
}

/// DDC0 sequence-number tracker (D10).
#[derive(Debug, Clone, Default)]
pub struct SeqTracker {
    expected: Option<u32>,
    stale_run: u32,
}

impl SeqTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Classify `seq` and advance the tracker (the D10 rules, in order).
    pub fn observe(&mut self, seq: u32) -> SeqEvent {
        let Some(expected) = self.expected else {
            return self.rebaseline(seq, SeqEvent::InOrder);
        };
        let delta = seq.wrapping_sub(expected) as i32;
        if delta == 0 {
            // Includes the 0xFFFF_FFFF -> 0 wrap.
            self.rebaseline(seq, SeqEvent::InOrder)
        } else if seq == 0 {
            // expected != 0 here: the radio began a new stream. Checked
            // before the gap rule so a restart is never ~4e9 losses.
            self.rebaseline(seq, SeqEvent::Restart)
        } else if delta > 0 {
            self.rebaseline(seq, SeqEvent::Gap(delta as u32))
        } else if delta >= -MAX_STALE_BEHIND {
            self.stale_run += 1;
            if self.stale_run >= STALE_RUN_RESTART {
                self.rebaseline(seq, SeqEvent::Restart)
            } else {
                // `expected` does not move: the datagram is discarded.
                SeqEvent::Stale
            }
        } else {
            self.rebaseline(seq, SeqEvent::Restart)
        }
    }

    /// Accept `seq` as the latest datagram and return `event`.
    fn rebaseline(&mut self, seq: u32, event: SeqEvent) -> SeqEvent {
        self.expected = Some(seq.wrapping_add(1));
        self.stale_run = 0;
        event
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every byte of `p` outside `set` is zero (D6: "all bytes not listed
    /// are 0").
    fn assert_rest_zero(p: &[u8], set: &[std::ops::Range<usize>]) {
        for (i, &b) in p.iter().enumerate() {
            if !set.iter().any(|r| r.contains(&i)) {
                assert_eq!(b, 0, "byte {i} should be 0");
            }
        }
    }

    fn orion() -> BoardInfo {
        board_info(Some(4))
    }

    #[test]
    fn general_packet_bytes() {
        let p = general_packet(0x0102_0304, &board_info(Some(5)));
        assert_eq!(p.len(), 60);
        assert_eq!(&p[0..4], &[0x01, 0x02, 0x03, 0x04]);
        assert_eq!(p[4], 0x00);
        assert_eq!(p[37], 0x08);
        assert_eq!(p[38], 0x01);
        assert_eq!(p[58], 0x00);
        assert_eq!(p[59], 0x03);
        assert_rest_zero(&p, &[0..4, 37..39, 59..60]);
        assert_eq!(general_packet(0, &board_info(Some(0)))[59], 0x00);
        assert_eq!(general_packet(0, &orion())[59], 0x01);
        assert_eq!(general_packet(0, &board_info(None))[59], 0x01);
    }

    #[test]
    fn rx_specific_packet_bytes_at_192k() {
        let p = rx_specific_packet(7, &orion(), 192);
        assert_eq!(p.len(), 1444);
        assert_eq!(&p[0..4], &[0, 0, 0, 7]);
        assert_eq!(p[4], 2, "Orion has two ADCs");
        assert_eq!(p[5], 0x00);
        assert_eq!(p[6], 0x00);
        assert_eq!(p[7], 0x01);
        assert_eq!(p[17], 0);
        assert_eq!(&p[18..20], &[0x00, 0xC0]);
        assert_eq!(p[22], 24);
        assert_rest_zero(&p, &[0..5, 7..8, 18..20, 22..23]);
        assert_eq!(rx_specific_packet(0, &board_info(Some(6)), 192)[4], 1);
    }

    #[test]
    fn rate_encodes_in_khz_for_every_rate() {
        for (hz, khz, bytes) in [
            (48_000.0, 48, [0x00, 0x30]),
            (96_000.0, 96, [0x00, 0x60]),
            (192_000.0, 192, [0x00, 0xC0]),
            (384_000.0, 384, [0x01, 0x80]),
        ] {
            assert_eq!(rate_khz(hz), Some(khz));
            assert_eq!(&rx_specific_packet(0, &orion(), khz)[18..20], &bytes);
        }
        for hz in [0.0, 44_100.0, 192_000.5, 191_999.0, 768_000.0, f64::NAN] {
            assert_eq!(rate_khz(hz), None, "{hz}");
        }
    }

    #[test]
    fn tx_specific_packet_bytes() {
        let p = tx_specific_packet(0xFFFF_FFFF);
        assert_eq!(p.len(), 60);
        assert_eq!(&p[0..4], &[0xFF; 4]);
        assert_eq!(p[4], 1);
        assert_rest_zero(&p, &[0..4, 4..5]);
    }

    #[test]
    fn high_priority_run_phase_word_and_filters() {
        let (a0, a1) = alex_words(orion().scheme, 7_030_000.0);
        let p = high_priority_packet(3, true, phase_word(7_030_000.0), a0, a1, 0);
        assert_eq!(p.len(), 1444);
        assert_eq!(&p[0..4], &[0, 0, 0, 3]);
        assert_eq!(p[4], 0x01);
        assert_eq!(&p[9..13], &[0x0E, 0xA5, 0x55, 0x55]);
        assert_eq!(&p[329..333], &p[9..13]);
        assert_eq!(p[345], 0);
        assert_eq!(&p[1428..1432], &0x0020_0000u32.to_be_bytes(), "alex1");
        assert_eq!(&p[1432..1436], &0x0020_0020u32.to_be_bytes(), "alex0");
        assert_eq!(p[1442], 0);
        assert_eq!(p[1443], 0);
        assert_rest_zero(&p, &[0..5, 9..13, 329..333, 1428..1436]);
    }

    #[test]
    fn high_priority_stop_clears_run_only() {
        let w = phase_word(14_000_000.0);
        let run = high_priority_packet(9, true, w, 0x0010_0002, 0x0010_0000, 12);
        let stop = high_priority_packet(9, false, w, 0x0010_0002, 0x0010_0000, 12);
        assert_eq!(stop[4], 0x00);
        let differ: Vec<usize> = (0..run.len()).filter(|&i| run[i] != stop[i]).collect();
        assert_eq!(differ, vec![4]);
    }

    #[test]
    fn high_priority_writes_adc0_step_attenuation_to_byte_1443() {
        let (a0, a1) = alex_words(orion().scheme, 7_030_000.0);
        let p = high_priority_packet(3, true, phase_word(7_030_000.0), a0, a1, 12);
        assert_eq!(p[1443], 12, "ADC0");
        assert_eq!(p[1442], 0, "ADC1");
        assert_rest_zero(&p, &[0..5, 9..13, 329..333, 1428..1436, 1443..1444]);
        let capped = high_priority_packet(3, true, 0, 0, 0, 40);
        assert_eq!(capped[1443], MAX_STEP_ATTENUATION_DB);
        assert_eq!(capped[1442], 0);
    }

    #[test]
    fn status_overload_bits_read_byte_5_of_a_60_byte_datagram() {
        assert_eq!(adc_overload_bits(&[0u8; 60]), Some(0));
        let mut d = [0u8; 60];
        d[5] = 0x03;
        assert_eq!(adc_overload_bits(&d), Some(0x03));
        assert_eq!(adc_overload_bits(&d[..59]), None);
        assert_eq!(adc_overload_bits(&[0x03u8; 61]), None);
    }

    #[test]
    fn phase_word_for_14_mhz() {
        assert_eq!(
            phase_word(14_000_000.0).to_be_bytes(),
            [0x1D, 0x2A, 0xAA, 0xAA]
        );
        assert_eq!(
            phase_word(7_030_000.0).to_be_bytes(),
            [0x0E, 0xA5, 0x55, 0x55]
        );
        // Exact integer floor(f * 2^32 / 122.88e6) across the valid range.
        for f in [1u64, 1875, 3_500_000, 10_106_000, 28_000_001, 61_439_999] {
            let exact = ((f << 32) / 122_880_000) as u32;
            assert_eq!(phase_word(f as f64), exact, "{f}");
        }
    }

    #[test]
    fn alex_words_classic_board() {
        let s = orion().scheme;
        assert_eq!(s, FilterScheme::Classic);
        assert_eq!(alex_words(s, 7_030_000.0), (0x0020_0020, 0x0020_0000));
        assert_eq!(alex_words(s, 14_012_300.0).0, 0x0010_0002);
        assert_eq!(alex_words(s, 14_012_300.0).1, 0x0010_0000);
        // Below 1.8 MHz the HPF is bypassed.
        assert_eq!(alex_words(s, 1_000_000.0), (0x0080_1000, 0x0080_0000));
    }

    #[test]
    fn alex_words_bandpass_board() {
        let dual = board_info(Some(5)).scheme;
        assert_eq!(dual, FilterScheme::BandPassDual);
        assert_eq!(alex_words(dual, 7_030_000.0), (0x0020_0010, 0x0020_1000));
        for id in [10, 11] {
            assert_eq!(board_info(Some(id)).scheme, FilterScheme::BandPassDual);
        }
        let g2e = board_info(Some(20)).scheme;
        assert_eq!(g2e, FilterScheme::BandPass);
        assert_eq!(alex_words(g2e, 7_030_000.0), (0x0020_0010, 0x0020_0000));
    }

    #[test]
    fn alex_words_unknown_board_bypass() {
        for id in [None, Some(7), Some(12), Some(99), Some(255)] {
            let b = board_info(id);
            assert_eq!(b.scheme, FilterScheme::Bypass, "{id:?}");
            for f in [1_000_000.0, 7_030_000.0, 50_313_000.0] {
                assert_eq!(alex_words(b.scheme, f), (0x2000_1000, 0x2000_0000));
            }
        }
    }

    #[test]
    fn no_filter_board_sends_zero_words() {
        for id in [0, 6] {
            let b = board_info(Some(id));
            assert_eq!(b.scheme, FilterScheme::None);
            assert_eq!(b.filter_enable, 0);
            assert_eq!(alex_words(b.scheme, 7_030_000.0), (0, 0));
            assert_eq!(alex_words(b.scheme, 50_313_000.0), (0, 0));
        }
    }

    #[test]
    fn board_table_names_adc_counts_and_enables() {
        use FilterScheme::*;
        let table: [(u8, &str, u8, u8, FilterScheme); 10] = [
            (0, "Atlas", 1, 0x00, None),
            (1, "Hermes", 1, 0x01, Classic),
            (2, "Hermes II", 1, 0x01, Classic),
            (3, "Angelia", 2, 0x01, Classic),
            (4, "Orion", 2, 0x01, Classic),
            (5, "Orion II", 2, 0x03, BandPassDual),
            (6, "Hermes Lite", 1, 0x00, None),
            (10, "Saturn", 2, 0x03, BandPassDual),
            (11, "Saturn", 2, 0x03, BandPassDual),
            (20, "G2E", 1, 0x01, BandPass),
        ];
        for (id, name, adc_count, filter_enable, scheme) in table {
            assert_eq!(
                board_info(Some(id)),
                BoardInfo {
                    id: Some(id),
                    name: Some(name),
                    adc_count,
                    filter_enable,
                    scheme,
                }
            );
        }
        for id in (0..=255u8).filter(|id| !table.iter().any(|row| row.0 == *id)) {
            let b = board_info(Some(id));
            assert_eq!((b.id, b.name), (Some(id), Option::None));
            assert_eq!((b.adc_count, b.filter_enable, b.scheme), (1, 0x01, Bypass));
        }
        let skipped = board_info(Option::None);
        assert_eq!(skipped.id, Option::None);
        assert_eq!(skipped.name, Option::None);
        assert_eq!((skipped.adc_count, skipped.filter_enable), (1, 0x01));
        assert_eq!(skipped.scheme, Bypass);
    }

    #[test]
    fn every_ladder_boundary() {
        // (threshold Hz, value 1 Hz below, value at and 1 Hz above).
        let hpf_rows = [
            (1_800_000.0, 0x1000, 0x40),
            (6_500_000.0, 0x40, 0x20),
            (9_500_000.0, 0x20, 0x10),
            (13_000_000.0, 0x10, 0x02),
            (20_000_000.0, 0x02, 0x04),
            (50_000_000.0, 0x04, 0x08),
        ];
        for (t, below, at_or_above) in hpf_rows {
            assert_eq!(hpf(t - 1.0), below, "HPF {t} - 1");
            assert_eq!(hpf(t), at_or_above, "HPF {t}");
            assert_eq!(hpf(t + 1.0), at_or_above, "HPF {t} + 1");
        }
        // LPF rows are "> threshold": (threshold, at and below, 1 Hz above).
        let lpf_rows = [
            (35_600_000.0, 0x4000_0000, 0x2000_0000),
            (24_000_000.0, 0x8000_0000, 0x4000_0000),
            (16_500_000.0, 0x0010_0000, 0x8000_0000),
            (8_000_000.0, 0x0020_0000, 0x0010_0000),
            (5_000_000.0, 0x0040_0000, 0x0020_0000),
            (2_500_000.0, 0x0080_0000, 0x0040_0000),
        ];
        for (t, at_or_below, above) in lpf_rows {
            assert_eq!(lpf(t - 1.0), at_or_below, "LPF {t} - 1");
            assert_eq!(lpf(t), at_or_below, "LPF {t}");
            assert_eq!(lpf(t + 1.0), above, "LPF {t} + 1");
        }
        let bpf_rows = [
            (1_500_000.0, 0x1000, 0x40),
            (2_100_000.0, 0x40, 0x20),
            (5_500_000.0, 0x20, 0x10),
            (11_000_000.0, 0x10, 0x02),
            (22_000_000.0, 0x02, 0x04),
            (35_000_000.0, 0x04, 0x08),
        ];
        for (t, below, at_or_above) in bpf_rows {
            assert_eq!(bpf(t - 1.0), below, "BPF {t} - 1");
            assert_eq!(bpf(t), at_or_above, "BPF {t}");
            assert_eq!(bpf(t + 1.0), at_or_above, "BPF {t} + 1");
        }
        // The ladders reach the words through each scheme.
        assert_eq!(
            alex_words(FilterScheme::Classic, 1_799_999.0).0,
            0x0080_1000
        );
        assert_eq!(
            alex_words(FilterScheme::BandPass, 35_600_001.0).0,
            0x2000_0008
        );
    }

    /// A valid datagram: sequence `seq`, then I = `i`, Q = `q` for pair 0
    /// and pair `k` = (k, -k) for the rest.
    fn datagram(seq: u32, i: [u8; 3], q: [u8; 3]) -> Vec<u8> {
        let mut d = vec![0u8; IQ_DATAGRAM_LEN];
        d[0..4].copy_from_slice(&seq.to_be_bytes());
        d[4..12].copy_from_slice(&[0xAA; 8]); // timestamp: ignored
        d[12..14].copy_from_slice(&24u16.to_be_bytes());
        d[14..16].copy_from_slice(&238u16.to_be_bytes());
        d[16..19].copy_from_slice(&i);
        d[19..22].copy_from_slice(&q);
        for k in 1..SAMPLES_PER_DATAGRAM {
            let at = 16 + 6 * k;
            let kk = k as i32;
            d[at..at + 3].copy_from_slice(&kk.to_be_bytes()[1..]);
            d[at + 3..at + 6].copy_from_slice(&(-kk).to_be_bytes()[1..]);
        }
        d
    }

    #[test]
    fn iq_datagram_parses_header_and_scales_by_2_pow_23() {
        let d = datagram(0xDEAD_BEEF, [0x7F, 0xFF, 0xFF], [0x80, 0x00, 0x00]);
        let mut out = vec![Complex32::new(9.0, 9.0)];
        assert_eq!(parse_iq_datagram(&d, &mut out), Ok(0xDEAD_BEEF));
        assert_eq!(out.len(), 1 + 238);
        assert_eq!(out[0], Complex32::new(9.0, 9.0), "existing samples kept");
        assert_eq!(out[1].re, 8_388_607.0 / 8_388_608.0);
        assert!((f64::from(out[1].re) - 0.999_999_88).abs() < 1e-7);
        assert_eq!(out[1].im, -1.0);
        for k in 1..238 {
            let v = k as f32 / 8_388_608.0;
            assert_eq!(out[1 + k], Complex32::new(v, -v), "pair {k}");
        }
        let d = datagram(0, [0xFF, 0xFF, 0xFF], [0x00, 0x00, 0x01]);
        let mut out = Vec::new();
        parse_iq_datagram(&d, &mut out).unwrap();
        assert_eq!(
            out[0],
            Complex32::new(-1.0 / 8_388_608.0, 1.0 / 8_388_608.0)
        );
    }

    #[test]
    fn iq_datagram_rejects_wrong_length_bits_or_count() {
        let good = datagram(5, [0, 0, 1], [0, 0, 1]);
        let mut out = vec![Complex32::new(1.0, 2.0)];
        let before = out.clone();
        for len in [0, 15, 1443] {
            assert_eq!(
                parse_iq_datagram(&good[..len], &mut out),
                Err(MalformedIq::Length(len))
            );
        }
        let mut long = good.clone();
        long.push(0);
        assert_eq!(
            parse_iq_datagram(&long, &mut out),
            Err(MalformedIq::Length(1445))
        );
        let mut bits = good.clone();
        bits[12..14].copy_from_slice(&16u16.to_be_bytes());
        assert_eq!(
            parse_iq_datagram(&bits, &mut out),
            Err(MalformedIq::BitsPerSample(16))
        );
        let mut count = good.clone();
        count[14..16].copy_from_slice(&237u16.to_be_bytes());
        assert_eq!(
            parse_iq_datagram(&count, &mut out),
            Err(MalformedIq::SamplesPerFrame(237))
        );
        assert_eq!(out, before, "a rejected datagram leaves `out` unchanged");
        assert_eq!(parse_iq_datagram(&good, &mut out), Ok(5));
    }

    fn observe_all(t: &mut SeqTracker, seqs: impl IntoIterator<Item = u32>) -> Vec<SeqEvent> {
        seqs.into_iter().map(|s| t.observe(s)).collect()
    }

    #[test]
    fn seq_in_order() {
        let mut t = SeqTracker::new();
        assert!(observe_all(&mut t, 0..100)
            .iter()
            .all(|e| *e == SeqEvent::InOrder));
        // The baseline need not be 0.
        let mut t = SeqTracker::new();
        assert_eq!(observe_all(&mut t, [1000, 1001]), [SeqEvent::InOrder; 2]);
    }

    #[test]
    fn seq_gap_of_50() {
        let mut t = SeqTracker::new();
        observe_all(&mut t, 0..100);
        assert_eq!(t.observe(150), SeqEvent::Gap(50));
        assert_eq!(t.observe(151), SeqEvent::InOrder);
    }

    #[test]
    fn seq_wrap_is_in_order() {
        let mut t = SeqTracker::new();
        assert_eq!(
            observe_all(&mut t, [0xFFFF_FFFE, 0xFFFF_FFFF, 0, 1]),
            [SeqEvent::InOrder; 4]
        );
    }

    #[test]
    fn seq_gap_across_wrap() {
        let mut t = SeqTracker::new();
        assert_eq!(t.observe(0xFFFF_FFFE), SeqEvent::InOrder);
        // 0xFFFF_FFFF, 0, 1 and 2 are missing.
        assert_eq!(t.observe(3), SeqEvent::Gap(4));
        assert_eq!(t.observe(4), SeqEvent::InOrder);
    }

    #[test]
    fn seq_stale_is_discarded_not_counted() {
        let mut t = SeqTracker::new();
        observe_all(&mut t, 100..110);
        assert_eq!(t.observe(120), SeqEvent::Gap(10));
        // Late arrivals from the gap, a duplicate, and the oldest still stale.
        assert_eq!(t.observe(115), SeqEvent::Stale);
        assert_eq!(t.observe(120), SeqEvent::Stale);
        assert_eq!(t.observe(121 - 64), SeqEvent::Stale);
        // `expected` did not move: 121 is next, in order.
        assert_eq!(t.observe(121), SeqEvent::InOrder);
        assert_eq!(t.observe(122), SeqEvent::InOrder);
    }

    #[test]
    fn restart_is_not_four_billion_losses() {
        let mut t = SeqTracker::new();
        observe_all(&mut t, 0..=30);
        assert_eq!(t.observe(0), SeqEvent::Restart);
        assert_eq!(t.observe(1), SeqEvent::InOrder);
        // A restart even where 0 would otherwise read as a forward gap.
        let mut t = SeqTracker::new();
        assert_eq!(t.observe(0xFFFF_FFF0), SeqEvent::InOrder);
        assert_eq!(t.observe(0), SeqEvent::Restart);
        assert_eq!(t.observe(1), SeqEvent::InOrder);
    }

    #[test]
    fn eight_stale_in_a_row_restart() {
        let mut t = SeqTracker::new();
        observe_all(&mut t, 100..=200);
        for s in 0..7 {
            assert_eq!(t.observe(190 + s), SeqEvent::Stale, "stale {s}");
        }
        assert_eq!(t.observe(197), SeqEvent::Restart);
        assert_eq!(t.observe(198), SeqEvent::InOrder, "197 is the new baseline");
        // A non-stale datagram resets the run.
        let mut t = SeqTracker::new();
        observe_all(&mut t, 100..=200);
        observe_all(&mut t, 190..197);
        assert_eq!(t.observe(201), SeqEvent::InOrder);
        assert_eq!(t.observe(150), SeqEvent::Stale);
    }

    #[test]
    fn far_backwards_is_a_restart() {
        let mut t = SeqTracker::new();
        observe_all(&mut t, 1000..=1100);
        // expected = 1101; 1101 - 65 is one further back than stale allows.
        assert_eq!(t.observe(1101 - 65), SeqEvent::Restart);
        assert_eq!(t.observe(1101 - 64), SeqEvent::InOrder);
    }
}
