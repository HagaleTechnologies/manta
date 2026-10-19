//! Two-tone FSK (RTTY) recognition, SPEC §2.6 (MAN-314).
//!
//! Fed one channelizer hop at a time with the same per-channel power and
//! noise floor the track manager uses. Every `BLOCK_HOPS` hops it judges
//! each hot channel pair `(j, j + d)` (frequency order, `d` in `LAGS`, no
//! wrap across Nyquist): RTTY keys exactly one of its two tones at any
//! instant, so the pair's per-hop SNR is anti-correlated, its envelope is
//! nearly constant (one tone is always on), each tone leads in a fair share
//! of hops, and the phase-difference frequency estimate puts the two tones
//! a standard shift apart. Passing blocks add evidence; contradicting
//! (positively correlated) blocks subtract it; a pair whose evidence
//! reaches the dwell is marked as RTTY. The track manager starts no CW
//! track on a marked signal's hard span and vetoes splatter-level spawns
//! in its shadow.
//!
//! Deterministic: no RNG, no clock; pairs iterate in frequency order and
//! every sum runs in index order with `f64` accumulators.

use crate::channelizer::{odd_channel_sign_correction, HopOutput};
use crate::floor::FloorBank;
use num_complex::{Complex32, Complex64};
use std::collections::BTreeMap;

/// One detector block: 1.000 s at 375 hops/s.
pub const BLOCK_HOPS: usize = 375;
/// Channel distances covering 170..850 Hz shifts at any tone offset.
const LAGS: [usize; 7] = [1, 2, 3, 4, 5, 9, 10];
/// Both channels' block-mean SNR must reach this to be judged.
const HOT_MEAN_DB: f64 = 6.0;
/// Pearson rho of the pair's per-hop SNR (dB) must be at most this.
const RHO_MAX: f64 = -0.5;
/// Constant envelope: `max(a, b) >= FILL_DB` in at least `FILL_MIN` of hops.
const FILL_DB: f32 = 10.0;
const FILL_MIN: f64 = 0.8;
/// Each tone leads the other by `DOM_DB` in at least `DOM_MIN` of hops.
const DOM_DB: f32 = 6.0;
const DOM_MIN: f64 = 0.1;
/// Hops at or above this SNR feed the phase-difference frequency estimator.
const PHASE_SNR_DB: f32 = 10.0;
/// Measured-shift windows that count as RTTY.
const SHIFT_WINDOWS_HZ: [(f64, f64); 3] = [(130.0, 230.0), (395.0, 475.0), (820.0, 885.0)];
/// Standard RTTY shifts; a signal reports the nearest as its nominal shift.
pub const STANDARD_SHIFTS_HZ: [f64; 5] = [170.0, 200.0, 425.0, 450.0, 850.0];
/// A pair's evidence (and mark) survives this many blocks without a pass.
pub const HOLD_BLOCKS: u32 = 30;
/// Hard span = `[low tone channel - GUARD, high tone channel + GUARD]`.
pub const GUARD_CHANNELS: usize = 1;
/// Spawn-only splatter shadow: the signal's `span_j` (not its hard span)
/// +/- this many channels ...
pub const SHADOW_WIDTH_CHANNELS: usize = 10;
/// ... for candidates more than this many dB below the RTTY's key-down level.
pub const SHADOW_DEPTH_DB: f64 = 40.0;
/// SPEC §1.1 channel spacing.
const CHANNEL_SPACING_HZ: f64 = 93.75;
/// hop = N/4 (SPEC §1.1), so a 2*pi phase advance per hop spans 4 channels.
const CHANNELS_PER_CYCLE: f64 = crate::channelizer::CHANNELS_PER_PHASE_CYCLE;

/// One marked RTTY signal. Channel positions `low_j`/`high_j` and `span_j`
/// are in frequency order (`j = (k + n/2) mod n`; `j = n/2` is the centre).
#[derive(Debug, Clone, PartialEq)]
pub struct FskSignal {
    /// Stable across blocks while the signal stays marked.
    pub id: u32,
    /// Space (lower tone) position, fractional channel.
    pub low_j: f64,
    /// Mark (upper tone) position, fractional channel.
    pub high_j: f64,
    pub measured_shift_hz: f64,
    /// Nearest of `STANDARD_SHIFTS_HZ`.
    pub nominal_shift_hz: f64,
    /// Key-down level, dB above the floor (block mean of the louder tone).
    pub level_db: f64,
    /// Channels the signal's passing pairs occupy, inclusive.
    pub span_j: (usize, usize),
    /// Hop index (`HopOutput::m`) of the block end that marked it.
    pub marked_hop: u64,
}

#[derive(Debug, Clone)]
struct PairState {
    evidence: u32,
    passes: u32,
    last_pass_block: u64,
    low_sum: f64,
    high_sum: f64,
    level_db: f64,
    marked: bool,
}

enum Verdict {
    Pass { low: f64, high: f64, level: f64 },
    Contra,
    Neutral,
}

/// SPEC §2.6 two-tone FSK detector.
pub struct FskDetector {
    n: usize,
    dwell_blocks: u32,
    /// Per-hop SNR (dB over the effective floor), row `j`, `BLOCK_HOPS` wide.
    buf: Vec<f32>,
    pos: usize,
    block: u64,
    /// Phase-difference accumulators, per `j`.
    z: Vec<Complex64>,
    /// Previous hop's sign-corrected channel output, per `j`.
    prev: Vec<Complex32>,
    mean: Vec<f64>,
    snr: Vec<f32>,
    pairs: BTreeMap<(usize, usize), PairState>,
    signals: Vec<FskSignal>,
    /// Hard-span membership, per FFT-order channel `k`.
    hard: Vec<bool>,
    next_id: u32,
}

impl FskDetector {
    /// A detector over `n_channels` channels that marks a pair after
    /// `dwell_blocks` passing blocks (at least 1).
    pub fn new(n_channels: usize, dwell_blocks: u32) -> Self {
        FskDetector {
            n: n_channels,
            dwell_blocks: dwell_blocks.max(1),
            buf: vec![0.0; n_channels * BLOCK_HOPS],
            pos: 0,
            block: 0,
            z: vec![Complex64::new(0.0, 0.0); n_channels],
            prev: vec![Complex32::new(0.0, 0.0); n_channels],
            mean: vec![0.0; n_channels],
            snr: vec![0.0; n_channels],
            pairs: BTreeMap::new(),
            signals: Vec::new(),
            hard: vec![false; n_channels],
            next_id: 1,
        }
    }

    /// Feeds one hop (`power_db[k]` = `channelizer::power_db(hop.power[k])`,
    /// `floor` already updated with it). Returns true when a block
    /// completed and changed the marked signals: one was marked or
    /// cleared, or a marked one's tones, shift, level or span moved.
    pub fn push_hop(&mut self, hop: &HopOutput, power_db: &[f64], floor: &FloorBank) -> bool {
        let mut snr = std::mem::take(&mut self.snr);
        for (k, s) in snr.iter_mut().enumerate() {
            *s = (power_db[k] - floor.effective_floor_db(k)) as f32;
        }
        let changed = self.push_snr(hop.m, &hop.x, &snr);
        self.snr = snr;
        changed
    }

    fn push_snr(&mut self, m: u64, x: &[Complex32], snr: &[f32]) -> bool {
        let n = self.n;
        for (k, &s) in snr.iter().enumerate().take(n) {
            let j = (k + n / 2) % n;
            self.buf[j * BLOCK_HOPS + self.pos] = s;
            // A hop without its complex spectrum (power-only test hops)
            // feeds no phase; the shape tests still run on `snr`.
            let xc = x.get(k).copied().unwrap_or_default() * odd_channel_sign_correction(m, k);
            if s >= PHASE_SNR_DB {
                let d = xc * self.prev[j].conj();
                self.z[j] += Complex64::new(d.re as f64, d.im as f64);
            }
            self.prev[j] = xc;
        }
        self.pos += 1;
        if self.pos < BLOCK_HOPS {
            return false;
        }
        self.pos = 0;
        let changed = self.end_block(m);
        self.z
            .iter_mut()
            .for_each(|z| *z = Complex64::new(0.0, 0.0));
        changed
    }

    /// Every currently marked signal, in frequency order.
    pub fn signals(&self) -> &[FskSignal] {
        &self.signals
    }

    /// Is FFT-order channel `k` inside a marked signal's hard span?
    pub fn in_hard_span(&self, k: usize) -> bool {
        self.hard[k]
    }

    /// Does a marked signal's shadow veto spawning a CW candidate on
    /// FFT-order channel `k` whose SNR is `snr_db`?
    pub fn shadow_vetoes_spawn(&self, k: usize, snr_db: f64) -> bool {
        let j = (k + self.n / 2) % self.n;
        self.signals.iter().any(|s| {
            j + SHADOW_WIDTH_CHANNELS >= s.span_j.0
                && j <= s.span_j.1 + SHADOW_WIDTH_CHANNELS
                && snr_db < s.level_db - SHADOW_DEPTH_DB
        })
    }

    fn row(&self, j: usize) -> &[f32] {
        &self.buf[j * BLOCK_HOPS..(j + 1) * BLOCK_HOPS]
    }

    fn end_block(&mut self, m: u64) -> bool {
        let n = self.n;
        for j in 0..n {
            let sum: f64 = self.row(j).iter().map(|&v| v as f64).sum();
            self.mean[j] = sum / BLOCK_HOPS as f64;
        }
        let block = self.block;
        for ja in 0..n {
            if self.mean[ja] < HOT_MEAN_DB {
                continue;
            }
            for d in LAGS {
                let jb = ja + d;
                if jb >= n {
                    break;
                }
                if self.mean[jb] < HOT_MEAN_DB {
                    continue;
                }
                match self.judge(ja, jb) {
                    Verdict::Pass { low, high, level } => {
                        let p = self.pairs.entry((ja, jb)).or_insert(PairState {
                            evidence: 0,
                            passes: 0,
                            last_pass_block: block,
                            low_sum: 0.0,
                            high_sum: 0.0,
                            level_db: level,
                            marked: false,
                        });
                        p.evidence += 1;
                        p.passes += 1;
                        p.last_pass_block = block;
                        p.low_sum += low;
                        p.high_sum += high;
                        p.level_db = level;
                    }
                    Verdict::Contra => {
                        if let Some(p) = self.pairs.get_mut(&(ja, jb)) {
                            p.evidence = p.evidence.saturating_sub(2);
                        }
                    }
                    Verdict::Neutral => {}
                }
            }
        }
        self.block += 1;
        let dwell = self.dwell_blocks;
        self.pairs
            .retain(|_, p| block + 1 - p.last_pass_block <= u64::from(HOLD_BLOCKS));
        for p in self.pairs.values_mut() {
            if p.evidence >= dwell {
                p.marked = true;
            }
        }
        self.regroup(m)
    }

    fn judge(&self, ja: usize, jb: usize) -> Verdict {
        let a = self.row(ja);
        let b = self.row(jb);
        let (ma, mb) = (self.mean[ja], self.mean[jb]);
        let (mut sab, mut saa, mut sbb, mut level) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
        let (mut fill, mut dom_a, mut dom_b) = (0usize, 0usize, 0usize);
        let mut lead_a = [false; BLOCK_HOPS];
        let mut lead_b = [false; BLOCK_HOPS];
        for i in 0..BLOCK_HOPS {
            let da = a[i] as f64 - ma;
            let db = b[i] as f64 - mb;
            sab += da * db;
            saa += da * da;
            sbb += db * db;
            let hi = a[i].max(b[i]);
            level += hi as f64;
            if hi >= FILL_DB {
                fill += 1;
            }
            if a[i] - b[i] >= DOM_DB {
                dom_a += 1;
                lead_a[i] = true;
            }
            if b[i] - a[i] >= DOM_DB {
                dom_b += 1;
                lead_b[i] = true;
            }
        }
        let rho = if saa > 0.0 && sbb > 0.0 {
            sab / (saa * sbb).sqrt()
        } else {
            0.0
        };
        if rho > 0.0 {
            return Verdict::Contra;
        }
        let frac = |c: usize| c as f64 / BLOCK_HOPS as f64;
        if rho > RHO_MAX || frac(fill) < FILL_MIN || frac(dom_a) < DOM_MIN || frac(dom_b) < DOM_MIN
        {
            return Verdict::Neutral;
        }
        let low = self.tone_pos(ja, &lead_a);
        let high = self.tone_pos(jb, &lead_b);
        let shift = (high - low) * CHANNEL_SPACING_HZ;
        if !SHIFT_WINDOWS_HZ
            .iter()
            .any(|&(lo, hi)| shift >= lo && shift <= hi)
        {
            return Verdict::Neutral;
        }
        Verdict::Pass {
            low,
            high,
            level: level / BLOCK_HOPS as f64,
        }
    }

    /// Fractional position of the tone channel `c` carries: the strongest
    /// of `c - 1 ..= c + 1` over the hops this tone leads, refined by that
    /// channel's mean phase advance per hop.
    fn tone_pos(&self, c: usize, lead: &[bool; BLOCK_HOPS]) -> f64 {
        let mut best = (c, f64::NEG_INFINITY);
        for cc in c.saturating_sub(1)..=(c + 1).min(self.n - 1) {
            let row = self.row(cc);
            let (mut sum, mut cnt) = (0.0f64, 0usize);
            for (i, &l) in lead.iter().enumerate() {
                if l {
                    sum += row[i] as f64;
                    cnt += 1;
                }
            }
            let mean = if cnt > 0 { sum / cnt as f64 } else { 0.0 };
            if mean > best.1 {
                best = (cc, mean);
            }
        }
        let z = self.z[best.0];
        let frac = if z.norm_sqr() > 0.0 {
            z.arg() / std::f64::consts::TAU * CHANNELS_PER_CYCLE
        } else {
            0.0
        };
        best.0 as f64 + frac
    }

    /// Groups marked pairs into signals, keeps ids stable, rebuilds the
    /// hard span. Returns whether any marked signal was added, removed or
    /// changed.
    fn regroup(&mut self, m: u64) -> bool {
        let n = self.n;
        let mut groups: Vec<Vec<(usize, usize)>> = Vec::new();
        let mut hi_edge = 0usize;
        for (&key, p) in &self.pairs {
            if !p.marked {
                continue;
            }
            match groups.last_mut() {
                Some(g) if key.0 <= hi_edge + 1 => {
                    g.push(key);
                    hi_edge = hi_edge.max(key.1);
                }
                _ => {
                    groups.push(vec![key]);
                    hi_edge = key.1;
                }
            }
        }
        let old = std::mem::take(&mut self.signals);
        let mut used = vec![false; old.len()];
        for g in groups {
            let mut rep = g[0];
            for &key in &g[1..] {
                if self.pairs[&key].passes > self.pairs[&rep].passes {
                    rep = key;
                }
            }
            let p = &self.pairs[&rep];
            let low_j = p.low_sum / p.passes as f64;
            let high_j = p.high_sum / p.passes as f64;
            let measured_shift_hz = (high_j - low_j) * CHANNEL_SPACING_HZ;
            let nominal_shift_hz = STANDARD_SHIFTS_HZ
                .iter()
                .copied()
                .min_by(|a, b| {
                    (a - measured_shift_hz)
                        .abs()
                        .total_cmp(&(b - measured_shift_hz).abs())
                })
                .unwrap_or(STANDARD_SHIFTS_HZ[0]);
            let lo = g.iter().map(|k| k.0).min().unwrap_or(rep.0);
            let hi = g.iter().map(|k| k.1).max().unwrap_or(rep.1);
            let lo = lo.min(low_j.floor().max(0.0) as usize);
            let hi = hi.max((high_j.ceil() as usize).min(n - 1));
            let prev = old
                .iter()
                .enumerate()
                .find(|(i, s)| !used[*i] && s.span_j.0 <= hi && lo <= s.span_j.1);
            let (id, marked_hop) = match prev {
                Some((i, s)) => {
                    used[i] = true;
                    (s.id, s.marked_hop)
                }
                None => {
                    let id = self.next_id;
                    self.next_id += 1;
                    (id, m)
                }
            };
            self.signals.push(FskSignal {
                id,
                low_j,
                high_j,
                measured_shift_hz,
                nominal_shift_hz,
                level_db: p.level_db,
                span_j: (lo, hi),
                marked_hop,
            });
        }
        self.hard.iter_mut().for_each(|h| *h = false);
        for s in &self.signals {
            let lo = s.span_j.0.saturating_sub(GUARD_CHANNELS);
            let hi = (s.span_j.1 + GUARD_CHANNELS).min(n - 1);
            for j in lo..=hi {
                self.hard[(j + n - n / 2) % n] = true;
            }
        }
        // A surviving id's metadata changes too (pass-averaged tones,
        // drift), and the track manager's RTTY log must follow it.
        old != self.signals
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const N: usize = 64;

    /// One hop of synthetic per-channel SNR/phase: channels at FFT index
    /// `k` for frequency-order `j`. `tones` = (j, snr_db, fractional
    /// offset in channels).
    fn hop(det: &mut FskDetector, m: u64, tones: &[(usize, f32, f64)]) -> bool {
        let mut snr = vec![0.0f32; N];
        let mut x = vec![Complex32::new(0.0, 0.0); N];
        for &(j, s, off) in tones {
            let k = (j + N - N / 2) % N;
            snr[k] = s;
            let ph = std::f64::consts::TAU * off / CHANNELS_PER_CYCLE * m as f64;
            let c = Complex32::from_polar(1.0, ph as f32);
            x[k] = c * odd_channel_sign_correction(m, k);
        }
        det.push_snr(m, &x, &snr)
    }

    /// Antiphase RTTY-like keying on `ja`/`jb` for `blocks` blocks.
    /// `duty_off` = fraction of each 16-hop cycle with both tones off.
    fn feed_pair(
        det: &mut FskDetector,
        m0: u64,
        blocks: usize,
        ja: (usize, f64),
        jb: (usize, f64),
        mode: &str,
    ) -> (u64, bool) {
        let mut m = m0;
        let mut changed = false;
        for _ in 0..blocks * BLOCK_HOPS {
            let phase = (m / 8).is_multiple_of(2);
            let tones: Vec<(usize, f32, f64)> = match mode {
                "rtty" => {
                    if phase {
                        vec![(ja.0, 30.0, ja.1)]
                    } else {
                        vec![(jb.0, 30.0, jb.1)]
                    }
                }
                "cw" => match (m / 8) % 4 {
                    0 => vec![(ja.0, 30.0, ja.1)],
                    1 => vec![(jb.0, 30.0, jb.1)],
                    _ => vec![],
                },
                "contra" => {
                    if phase {
                        vec![(ja.0, 30.0, ja.1), (jb.0, 30.0, jb.1)]
                    } else {
                        vec![]
                    }
                }
                _ => vec![],
            };
            changed |= hop(det, m, &tones);
            m += 1;
        }
        (m, changed)
    }

    /// 170 Hz shift: low tone centred on channel 20, high tone 1.813
    /// channels above (channel 22, -0.187).
    const LOW: (usize, f64) = (20, 0.0);
    const HIGH: (usize, f64) = (22, 170.0 / CHANNEL_SPACING_HZ - 2.0);

    #[test]
    fn anti_correlated_pair_passes_a_block() {
        let mut det = FskDetector::new(N, 20);
        feed_pair(&mut det, 0, 1, LOW, HIGH, "rtty");
        let p = det.pairs.get(&(20, 22)).expect("pair passed");
        assert_eq!(p.passes, 1);
        let shift = (p.high_sum - p.low_sum) * CHANNEL_SPACING_HZ;
        assert!((shift - 170.0).abs() < 1.0, "measured shift {shift}");
    }

    #[test]
    fn cw_like_on_off_pair_fails_fill() {
        let mut det = FskDetector::new(N, 20);
        feed_pair(&mut det, 0, 3, LOW, HIGH, "cw");
        assert!(det.pairs.is_empty());
    }

    #[test]
    fn positively_correlated_pair_is_contra_and_subtracts_two() {
        let mut det = FskDetector::new(N, 20);
        let (m, _) = feed_pair(&mut det, 0, 3, LOW, HIGH, "rtty");
        assert_eq!(det.pairs[&(20, 22)].evidence, 3);
        feed_pair(&mut det, m, 1, LOW, HIGH, "contra");
        assert_eq!(det.pairs[&(20, 22)].evidence, 1);
    }

    #[test]
    fn marks_after_dwell_blocks_of_passes() {
        let mut det = FskDetector::new(N, 20);
        let (m, changed) = feed_pair(&mut det, 0, 19, LOW, HIGH, "rtty");
        assert!(!changed);
        assert!(det.signals().is_empty());
        let (_, changed) = feed_pair(&mut det, m, 1, LOW, HIGH, "rtty");
        assert!(changed);
        assert_eq!(det.signals().len(), 1);
        let s = &det.signals()[0];
        assert_eq!(s.nominal_shift_hz, 170.0);
        assert_eq!(s.marked_hop, 20 * BLOCK_HOPS as u64 - 1);
    }

    #[test]
    fn marked_signal_moving_reports_a_change() {
        let mut det = FskDetector::new(N, 2);
        let (m, changed) = feed_pair(&mut det, 0, 2, LOW, HIGH, "rtty");
        assert!(changed);
        let before = det.signals()[0].clone();
        let (_, changed) = feed_pair(&mut det, m, 1, (20, 0.25), (22, HIGH.1 + 0.25), "rtty");
        assert!(changed, "same id, moved tones");
        let after = &det.signals()[0];
        assert_eq!(after.id, before.id);
        assert!(
            after.low_j > before.low_j,
            "{} vs {}",
            after.low_j,
            before.low_j
        );
    }

    #[test]
    fn neutral_blocks_do_not_reset_evidence() {
        let mut det = FskDetector::new(N, 20);
        let (m, _) = feed_pair(&mut det, 0, 5, LOW, HIGH, "rtty");
        let (m, _) = feed_pair(&mut det, m, 10, LOW, HIGH, "silent");
        assert_eq!(det.pairs[&(20, 22)].evidence, 5);
        feed_pair(&mut det, m, 15, LOW, HIGH, "rtty");
        assert_eq!(det.signals().len(), 1);
    }

    #[test]
    fn evidence_expires_after_hold_blocks_without_pass() {
        let mut det = FskDetector::new(N, 20);
        let (m, _) = feed_pair(&mut det, 0, 20, LOW, HIGH, "rtty");
        assert_eq!(det.signals().len(), 1);
        let (m, _) = feed_pair(&mut det, m, HOLD_BLOCKS as usize - 1, LOW, HIGH, "silent");
        assert_eq!(det.signals().len(), 1, "still held");
        let (_, changed) = feed_pair(&mut det, m, 1, LOW, HIGH, "silent");
        assert!(changed);
        assert!(det.signals().is_empty());
        assert!(det.pairs.is_empty());
    }

    #[test]
    fn shift_outside_windows_never_marks() {
        for shift in [300.0, 600.0] {
            let d = (shift / CHANNEL_SPACING_HZ) as usize;
            let high = (20 + d, shift / CHANNEL_SPACING_HZ - d as f64);
            let mut det = FskDetector::new(N, 5);
            feed_pair(&mut det, 0, 10, LOW, high, "rtty");
            assert!(det.signals().is_empty(), "shift {shift} marked");
        }
    }

    #[test]
    fn suppressed_span_is_tones_plus_one_channel_guard() {
        let mut det = FskDetector::new(N, 2);
        feed_pair(&mut det, 0, 2, LOW, HIGH, "rtty");
        assert_eq!(det.signals()[0].span_j, (20, 22));
        let hard: Vec<usize> = (0..N)
            .map(|j| (j + N - N / 2) % N)
            .enumerate()
            .filter(|&(_, k)| det.in_hard_span(k))
            .map(|(j, _)| j)
            .collect();
        assert_eq!(hard, vec![19, 20, 21, 22, 23]);
    }

    #[test]
    fn shadow_veto_only_inside_width_and_below_depth() {
        let mut det = FskDetector::new(N, 2);
        feed_pair(&mut det, 0, 2, LOW, HIGH, "rtty");
        let level = det.signals()[0].level_db;
        let k = |j: usize| (j + N - N / 2) % N;
        let weak = level - SHADOW_DEPTH_DB - 1.0;
        let strong = level - SHADOW_DEPTH_DB + 1.0;
        assert!(det.shadow_vetoes_spawn(k(32), weak));
        assert!(det.shadow_vetoes_spawn(k(10), weak));
        assert!(!det.shadow_vetoes_spawn(k(33), weak));
        assert!(!det.shadow_vetoes_spawn(k(9), weak));
        assert!(!det.shadow_vetoes_spawn(k(30), strong));
    }

    #[test]
    fn deterministic_across_runs() {
        let run = || {
            let mut det = FskDetector::new(N, 3);
            feed_pair(&mut det, 0, 4, LOW, HIGH, "rtty");
            (det.signals().to_vec(), det.hard.clone())
        };
        assert_eq!(run(), run());
    }
}
