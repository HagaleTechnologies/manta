//! MAN-204: estimate a strong keyed carrier's received waveform from raw IQ
//! and subtract it from a separate buffer, plus the two guards that decide
//! whether a subtraction is worth making: the second-carrier evidence in the
//! strong station's key-up gaps and the residual check. Offline only: nothing
//! in the live decode path calls this module. See
//! docs/DECISIONS/2026-11-03-man204-guarded-two-stage-cancellation.md.
//!
//! Determinism (SPEC §6): no RNG and no clock outside the tests; every sum
//! over samples accumulates in `f64`/`Complex64`, sequentially; every phase
//! argument is reduced with `rem_euclid` in `f64` before any narrowing.

use num_complex::{Complex32, Complex64};
use std::f64::consts::{PI, TAU};

/// Moving-average window of the edge finder and the block length of the
/// residual check and the evidence: 256 samples at 96 kS/s (128 at 48,
/// 512 at 192). Plan table 3: edge rms 0.224 ms beside a 20 dB weaker
/// neighbour, against 0.377 ms from the channelizer output.
pub const EDGE_WINDOW_S: f64 = 256.0 / 96_000.0;
/// Modelled raised-cosine rise and fall, 0 to 100 %, centred on the 50 %
/// crossing (SPEC §7's 5 ms keyer).
pub const RISE_S: f64 = 5e-3;
/// Fewest fitted elements an estimate needs before the engine uses it.
pub const MIN_ELEMENTS: usize = 20;
/// Largest carrier refinement step the engine accepts. Measured steps are
/// 0.3 Hz from a spot hint and 3 Hz from a forced hint (plan table 2).
pub const MAX_CARRIER_STEP_HZ: f64 = 5.0;
/// Shortest element interior (`[start + RISE_S, end - RISE_S]`) that is fitted.
pub const MIN_INTERIOR_S: f64 = 10e-3;
/// Block length of the per-element phase-slope fit.
pub const SLOPE_BLOCK_S: f64 = 0.5e-3;
/// Blocks this close to either end of the buffer are not classified.
pub const RIM_S: f64 = 0.2;
/// Half-width of the residual check's edge zone: the modelled ramp itself.
/// A ±`RISE_S` zone (the plan's first choice) puts the median edge block
/// outside the ramp, where a 0.67 ms edge shift leaves nothing: it read
/// -6.6 dB there, against +3.5 dB with this zone.
pub const EDGE_ZONE_S: f64 = RISE_S / 2.0;
/// Percentile of the edge finder's amplitudes taken as key-down level.
pub const EDGE_PERCENTILE: f64 = 0.98;

/// The residual check accepts when the median edge block, over the
/// neighbour level, is at most this. Plan table 7 put every testkit pair
/// cell that kept 8 or 9 of 9 weak calls at -6.5 dB or lower and every cell
/// that lost B at -3.8 dB or higher. With the `EDGE_ZONE_S` zone, this
/// module's synthetic pair (seeds 204, 2, 3) reads -14.7 to -15.3 dB clean,
/// -9.3 to -11.1 dB at 0.2 ms jitter, -5.8 to -6.3 dB at 0.4 ms jitter (a
/// cell that lost B in table 7, so rule A puts it on the dirty side),
/// -2.2 to -2.4 dB at a 0.33 ms shift and +3.5 dB or more at 0.67 ms.
pub const CLEAN_EDGE_OVER_NEIGHBOUR_DB: f64 = -7.0;
/// The residual check accepts when the median interior block, over the
/// neighbour level, is at most this. Plan table 7: good cells -7.8 dB or
/// lower; a 3 Hz carrier error -1.0 dB. This module's synthetic pair reads
/// -18.3 to -18.8 dB clean.
pub const CLEAN_INTERIOR_OVER_NEIGHBOUR_DB: f64 = -4.0;
/// Percentile of gap-block residual power taken as the neighbour level. A
/// `TwoSignalCw` candidate has at least 15 % hot gap blocks, so it lands on
/// the neighbour's key-down blocks (plan table 7).
pub const NEIGHBOUR_PERCENTILE: f64 = 0.90;
/// Below this hot fraction the strong station is alone. Plan table 6: a
/// lone station 5.9 to 6.6 % (noise expectation 6.3 %); a keyed neighbour
/// 23.8 % at -5 dB, 41.7 to 45.7 % at 0 dB and above.
pub const ONE_SIGNAL_MAX_HOT: f64 = 0.15;
/// Above this hot fraction the gaps hold a steady (non-CW) carrier. Plan
/// table 6: keyed neighbours at most 45.7 %; steady carriers 86.7 to 100 %.
pub const NON_CW_MIN_HOT: f64 = 0.75;
/// Least phase coherence of the hot gap-block pairs for `TwoSignalCw`.
/// Plan table 6: keyed and steady neighbours 0.86 to 0.99; a lone station 0.
pub const MIN_COHERENCE: f64 = 0.8;
/// Smallest neighbour offset treated as a separate emitter.
pub const MIN_OFFSET_HZ: f64 = 10.0;
/// Largest neighbour offset treated as local (the block rate aliases at
/// half of `1 / EDGE_WINDOW_S`, 187.5 Hz).
pub const MAX_OFFSET_HZ: f64 = 150.0;
/// A gap block is hot above this multiple of the ±500 Hz reference (6 dB).
pub const HOT_FACTOR: f64 = 4.0;
/// Both blocks of an offset pair sit above this multiple of the reference
/// (10 dB).
pub const PAIR_FACTOR: f64 = 10.0;
/// Offset of the evidence's noise reference either side of the carrier.
pub const REFERENCE_OFFSET_HZ: f64 = 500.0;

/// One key-down element: raw-sample times of its 50 % rising and falling
/// crossings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeyedElement {
    pub start: f64,
    pub end: f64,
}

/// An element's fitted complex gain (at `ref_sample`) and phase slope,
/// both relative to the estimate's carrier.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ElementFit {
    pub element: KeyedElement,
    pub gain: Complex64,
    pub slope_rad_per_sample: f64,
    pub ref_sample: f64,
}

/// A strong keyed station's estimated received waveform.
#[derive(Debug, Clone, PartialEq)]
pub struct StrongEstimate {
    /// Offset from the buffer's centre, after one refinement.
    pub carrier_hz: f64,
    /// Refined carrier minus the hint.
    pub carrier_step_hz: f64,
    pub fits: Vec<ElementFit>,
}

/// What a subtraction of the estimate would leave in the strong station's
/// own narrowband, relative to the neighbour it is meant to uncover.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResidualCheck {
    pub edge_over_neighbour_db: f64,
    pub interior_over_neighbour_db: f64,
    pub edge_blocks: usize,
    pub interior_blocks: usize,
    pub gap_blocks: usize,
}

impl ResidualCheck {
    /// Both ratios at or under their thresholds.
    pub fn clean(&self) -> bool {
        self.edge_over_neighbour_db <= CLEAN_EDGE_OVER_NEIGHBOUR_DB
            && self.interior_over_neighbour_db <= CLEAN_INTERIOR_OVER_NEIGHBOUR_DB
    }
}

/// What occupies the strong station's key-up gaps in the original buffer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SecondCarrierEvidence {
    pub gap_blocks: usize,
    /// Share of gap blocks over `HOT_FACTOR` times the ±500 Hz reference.
    pub hot_fraction: f64,
    /// Second carrier's offset from the strong carrier, from the lag-1
    /// products of consecutive gap blocks over `PAIR_FACTOR` times the
    /// reference; `None` when no such pair exists.
    pub offset_hz: Option<f64>,
    /// `|Σz| / Σ|z|` of those products; 0 when there are none.
    pub coherence: f64,
}

/// The local hypothesis the evidence supports.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Hypothesis {
    OneSignal,
    TwoSignalCw { offset_hz: f64 },
    NonCw,
    Unresolved,
}

/// `e^{+j 2π cyc k}` with the phase reduced in f64 before evaluation.
fn rotor(cycles_per_sample: f64, k: usize) -> Complex64 {
    let ph = (cycles_per_sample * k as f64).rem_euclid(1.0) * TAU;
    Complex64::new(ph.cos(), ph.sin())
}

/// `iq[k] · e^{-j 2π (carrier_hz / fs) k}` in `Complex64`.
fn derotated(iq: &[Complex32], fs: f64, carrier_hz: f64, k: usize) -> Complex64 {
    let x = Complex64::new(iq[k].re as f64, iq[k].im as f64);
    x * rotor(carrier_hz / fs, k).conj()
}

/// Edge-window length in samples (a multiple of 8) and its step.
fn edge_window(fs: f64) -> (usize, usize) {
    let step = ((fs * EDGE_WINDOW_S) / 8.0).round().max(1.0) as usize;
    (step * 8, step)
}

/// Means of the derotated input over consecutive, non-overlapping blocks of
/// `len` samples (a trailing partial block is dropped).
fn block_means(iq: &[Complex32], fs: f64, carrier_hz: f64, len: usize) -> Vec<Complex64> {
    let blocks = iq.len() / len;
    let mut out = Vec::with_capacity(blocks);
    for b in 0..blocks {
        let mut acc = Complex64::new(0.0, 0.0);
        for k in b * len..(b + 1) * len {
            acc += derotated(iq, fs, carrier_hz, k);
        }
        out.push(acc / len as f64);
    }
    out
}

/// `p`-th percentile (nearest rank) of `v`, sorted in place; `None` if empty.
fn percentile(v: &mut [f64], p: f64) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let idx = ((v.len() - 1) as f64 * p).round() as usize;
    Some(v[idx.min(v.len() - 1)])
}

/// Raised-cosine step of width `RISE_S` centred on `t = 0` (seconds).
fn rc(t: f64) -> f64 {
    if t <= -RISE_S / 2.0 {
        0.0
    } else if t >= RISE_S / 2.0 {
        1.0
    } else {
        0.5 + 0.5 * (PI * t / RISE_S).sin()
    }
}

/// Key-down elements of the carrier at `carrier_hz` (offset from centre):
/// half-amplitude crossings of a centred `EDGE_WINDOW_S` moving average of
/// the derotated input, evaluated every window/8 samples. Only complete
/// elements (a rising crossing then a falling one) at least `RISE_S` long.
pub fn find_elements(iq: &[Complex32], fs: f64, carrier_hz: f64) -> Vec<KeyedElement> {
    let (window, step) = edge_window(fs);
    if iq.len() < window {
        return Vec::new();
    }
    let subs = block_means(iq, fs, carrier_hz, step);
    if subs.len() < 8 {
        return Vec::new();
    }
    let mut amps = Vec::with_capacity(subs.len() - 7);
    let mut run: Complex64 = subs[..8].iter().sum();
    amps.push((run / 8.0).norm());
    for i in 8..subs.len() {
        run += subs[i] - subs[i - 8];
        amps.push((run / 8.0).norm());
    }
    let mut sorted = amps.clone();
    let Some(top) = percentile(&mut sorted, EDGE_PERCENTILE) else {
        return Vec::new();
    };
    let thr = 0.5 * top;
    if thr.is_nan() || thr <= 0.0 {
        return Vec::new();
    }
    // Output i covers samples [i·step, i·step + window): centre below.
    let centre = |i: usize| (i * step) as f64 + (window as f64 - 1.0) / 2.0;
    let min_len = RISE_S * fs;
    let mut out = Vec::new();
    let mut rising: Option<f64> = None;
    for i in 1..amps.len() {
        let (a0, a1) = (amps[i - 1], amps[i]);
        let cross = || centre(i - 1) + (thr - a0) / (a1 - a0) * step as f64;
        if a0 < thr && a1 >= thr {
            rising = Some(cross());
        } else if a0 >= thr && a1 < thr {
            if let Some(start) = rising.take() {
                let end = cross();
                if end - start >= min_len {
                    out.push(KeyedElement { start, end });
                }
            }
        }
    }
    out
}

/// Fit each element's complex gain and phase slope at `carrier_hz`. An
/// element whose interior `[start + RISE_S, end - RISE_S]` is shorter than
/// `MIN_INTERIOR_S` or leaves the buffer is skipped.
pub fn fit_elements(
    iq: &[Complex32],
    fs: f64,
    carrier_hz: f64,
    els: &[KeyedElement],
) -> Vec<ElementFit> {
    let rise = RISE_S * fs;
    let min_len = (MIN_INTERIOR_S * fs).ceil() as usize;
    let block = (SLOPE_BLOCK_S * fs).round().max(1.0) as usize;
    let mut out = Vec::new();
    for el in els {
        let lo = (el.start + rise).ceil();
        let hi = (el.end - rise).floor();
        if lo < 0.0 || hi > iq.len() as f64 || hi - lo < min_len as f64 {
            continue;
        }
        let (i0, i1) = (lo as usize, hi as usize);
        let len = i1 - i0;
        let ref_sample = (i0 + i1 - 1) as f64 / 2.0;
        // Weighted least squares of the unwrapped block-mean phase.
        let mut pts: Vec<(f64, f64, f64)> = Vec::new();
        let mut prev: Option<Complex64> = None;
        let mut phase = 0.0;
        for b in 0..len / block {
            let mut acc = Complex64::new(0.0, 0.0);
            for k in i0 + b * block..i0 + (b + 1) * block {
                acc += derotated(iq, fs, carrier_hz, k);
            }
            let z = acc / block as f64;
            if let Some(p) = prev {
                phase += (z * p.conj()).arg();
            } else {
                phase = z.arg();
            }
            prev = Some(z);
            let t = (i0 + b * block) as f64 + (block as f64 - 1.0) / 2.0;
            pts.push((t, phase, z.norm()));
        }
        let wsum: f64 = pts.iter().map(|p| p.2).sum();
        let slope = if pts.len() >= 2 && wsum > 0.0 {
            let tm = pts.iter().map(|p| p.2 * p.0).sum::<f64>() / wsum;
            let pm = pts.iter().map(|p| p.2 * p.1).sum::<f64>() / wsum;
            let num: f64 = pts.iter().map(|p| p.2 * (p.0 - tm) * (p.1 - pm)).sum();
            let den: f64 = pts.iter().map(|p| p.2 * (p.0 - tm) * (p.0 - tm)).sum();
            if den > 0.0 {
                num / den
            } else {
                0.0
            }
        } else {
            0.0
        };
        // Hann-weighted mean with the slope removed: the gain at ref_sample.
        let mut acc = Complex64::new(0.0, 0.0);
        let mut wacc = 0.0;
        for k in i0..i1 {
            let w = 0.5 - 0.5 * (TAU * ((k - i0) as f64 + 0.5) / len as f64).cos();
            let tilt = (-slope * (k as f64 - ref_sample)).rem_euclid(TAU);
            acc += derotated(iq, fs, carrier_hz, k) * Complex64::from_polar(w, tilt);
            wacc += w;
        }
        if wacc <= 0.0 {
            continue;
        }
        out.push(ElementFit {
            element: *el,
            gain: acc / wacc,
            slope_rad_per_sample: slope,
            ref_sample,
        });
    }
    out
}

/// Weighted mean phase slope of `fits` in Hz (weight = interior length ×
/// |gain|); 0 with no fits.
fn mean_slope_hz(fits: &[ElementFit], fs: f64) -> f64 {
    let mut num = 0.0;
    let mut den = 0.0;
    for f in fits {
        let w = (f.element.end - f.element.start - 2.0 * RISE_S * fs).max(0.0) * f.gain.norm();
        num += w * f.slope_rad_per_sample;
        den += w;
    }
    if den > 0.0 {
        num / den * fs / TAU
    } else {
        0.0
    }
}

/// Find, fit, refine the carrier once by the fits' weighted mean slope,
/// then re-find and refit at the refined carrier.
pub fn estimate_strong(iq: &[Complex32], fs: f64, carrier_hint_hz: f64) -> StrongEstimate {
    let els = find_elements(iq, fs, carrier_hint_hz);
    let fits = fit_elements(iq, fs, carrier_hint_hz, &els);
    let step = mean_slope_hz(&fits, fs);
    let carrier_hz = carrier_hint_hz + step;
    let els = find_elements(iq, fs, carrier_hz);
    let fits = fit_elements(iq, fs, carrier_hz, &els);
    StrongEstimate {
        carrier_hz,
        carrier_step_hz: step,
        fits,
    }
}

/// Sample span `[lo, hi)` a fit's synthesis covers, clipped to `n`.
fn fit_span(f: &ElementFit, fs: f64, n: usize) -> (usize, usize) {
    let pad = 2.0 * RISE_S * fs;
    let lo = (f.element.start - pad).floor().max(0.0) as usize;
    let hi = ((f.element.end + pad).ceil().max(0.0) as usize).min(n);
    (lo.min(hi), hi)
}

/// The fit's synthesis at sample `k`, derotated to the carrier.
fn synth_baseband(f: &ElementFit, fs: f64, k: usize) -> Complex64 {
    let env = rc((k as f64 - f.element.start) / fs) - rc((k as f64 - f.element.end) / fs);
    if env == 0.0 {
        return Complex64::new(0.0, 0.0);
    }
    let tilt = (f.slope_rad_per_sample * (k as f64 - f.ref_sample)).rem_euclid(TAU);
    f.gain * Complex64::from_polar(env, tilt)
}

/// `buf[k] -= synth[k]` over each element's span, f64 phase.
pub fn subtract(buf: &mut [Complex32], fs: f64, est: &StrongEstimate) {
    let cyc = est.carrier_hz / fs;
    for f in &est.fits {
        let (lo, hi) = fit_span(f, fs, buf.len());
        for (k, x) in buf.iter_mut().enumerate().take(hi).skip(lo) {
            let s = synth_baseband(f, fs, k) * rotor(cyc, k);
            *x -= Complex32::new(s.re as f32, s.im as f32);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum BlockClass {
    Edge,
    Interior,
    Gap,
    Other,
}

/// Class of each `len`-sample block by its centre: edge within `EDGE_ZONE_S` of
/// an edge, interior more than `2·RISE_S` inside an element, gap more than
/// `3·RISE_S` from every element; blocks within `RIM_S` of either end are
/// `Other`. The one definition both measurements share.
fn classify_blocks(
    blocks: usize,
    len: usize,
    fs: f64,
    n: usize,
    fits: &[ElementFit],
) -> Vec<BlockClass> {
    let r = RISE_S * fs;
    let centre = |b: usize| (b * len) as f64 + (len as f64 - 1.0) / 2.0;
    let mut class = vec![BlockClass::Gap; blocks];
    let range = |lo: f64, hi: f64| {
        let a = ((lo - (len as f64 - 1.0) / 2.0) / len as f64)
            .ceil()
            .max(0.0) as usize;
        let b = (((hi - (len as f64 - 1.0) / 2.0) / len as f64).floor() + 1.0).max(0.0) as usize;
        (a.min(blocks), b.min(blocks))
    };
    for f in fits {
        let (a, b) = range(f.element.start - 3.0 * r, f.element.end + 3.0 * r);
        for c in &mut class[a..b.max(a)] {
            *c = BlockClass::Other;
        }
    }
    for f in fits {
        let (s, e) = (f.element.start, f.element.end);
        if e - s > 4.0 * r {
            let (a, b) = range(s + 2.0 * r, e - 2.0 * r);
            for (i, cls) in class.iter_mut().enumerate().take(b.max(a)).skip(a) {
                let c = centre(i);
                if c > s + 2.0 * r && c < e - 2.0 * r {
                    *cls = BlockClass::Interior;
                }
            }
        }
        for edge in [s, e] {
            let (a, b) = range(edge - EDGE_ZONE_S * fs, edge + EDGE_ZONE_S * fs);
            for c in &mut class[a..b.max(a)] {
                *c = BlockClass::Edge;
            }
        }
    }
    let rim = RIM_S * fs;
    for (b, c) in class.iter_mut().enumerate() {
        let x = centre(b);
        if x < rim || x > n as f64 - rim {
            *c = BlockClass::Other;
        }
    }
    class
}

/// `10·log10(num / den)`, `+∞` when either side is missing.
fn ratio_db(num: Option<f64>, den: Option<f64>) -> f64 {
    match (num, den) {
        (Some(n), Some(d)) if d > 0.0 => 10.0 * (n.max(f64::MIN_POSITIVE) / d).log10(),
        _ => f64::INFINITY,
    }
}

/// What subtracting `est` would leave in the strong station's narrowband,
/// computed blockwise from the fits (no residual buffer is built): the
/// median edge and interior block powers over the `NEIGHBOUR_PERCENTILE`
/// gap block power.
pub fn residual_check(iq: &[Complex32], fs: f64, est: &StrongEstimate) -> ResidualCheck {
    let (len, _) = edge_window(fs);
    let mut z = block_means(iq, fs, est.carrier_hz, len);
    let blocks = z.len();
    for f in &est.fits {
        let (lo, hi) = fit_span(f, fs, blocks * len);
        for k in lo..hi {
            z[k / len] -= synth_baseband(f, fs, k) / len as f64;
        }
    }
    let class = classify_blocks(blocks, len, fs, iq.len(), &est.fits);
    let (mut edge, mut interior, mut gap) = (Vec::new(), Vec::new(), Vec::new());
    for (zb, c) in z.iter().zip(&class) {
        let p = zb.norm_sqr();
        match c {
            BlockClass::Edge => edge.push(p),
            BlockClass::Interior => interior.push(p),
            BlockClass::Gap => gap.push(p),
            BlockClass::Other => {}
        }
    }
    let nb = percentile(&mut gap, NEIGHBOUR_PERCENTILE).map(|v| v.max(f64::MIN_POSITIVE));
    ResidualCheck {
        edge_over_neighbour_db: ratio_db(percentile(&mut edge, 0.5), nb),
        interior_over_neighbour_db: ratio_db(percentile(&mut interior, 0.5), nb),
        edge_blocks: edge.len(),
        interior_blocks: interior.len(),
        gap_blocks: gap.len(),
    }
}

/// Second-carrier evidence in the strong station's key-up gaps of the
/// original buffer. The reference is the median gap-block power at the
/// carrier ± `REFERENCE_OFFSET_HZ`, pooled, floored at 1e-12 × the median
/// interior-block power at the carrier.
pub fn second_carrier_evidence(
    iq: &[Complex32],
    fs: f64,
    est: &StrongEstimate,
) -> SecondCarrierEvidence {
    let (len, _) = edge_window(fs);
    let z = block_means(iq, fs, est.carrier_hz, len);
    let class = classify_blocks(z.len(), len, fs, iq.len(), &est.fits);
    let gap_idx: Vec<usize> = (0..z.len())
        .filter(|&b| class[b] == BlockClass::Gap)
        .collect();
    let mut interior: Vec<f64> = (0..z.len())
        .filter(|&b| class[b] == BlockClass::Interior)
        .map(|b| z[b].norm_sqr())
        .collect();
    let mut side = Vec::with_capacity(2 * gap_idx.len());
    for off in [-REFERENCE_OFFSET_HZ, REFERENCE_OFFSET_HZ] {
        let cyc = est.carrier_hz + off;
        for &b in &gap_idx {
            let mut acc = Complex64::new(0.0, 0.0);
            for k in b * len..(b + 1) * len {
                acc += derotated(iq, fs, cyc, k);
            }
            side.push((acc / len as f64).norm_sqr());
        }
    }
    let floor = 1e-12 * percentile(&mut interior, 0.5).unwrap_or(0.0);
    let reference = percentile(&mut side, 0.5)
        .unwrap_or(0.0)
        .max(floor)
        .max(f64::MIN_POSITIVE);
    let hot = gap_idx
        .iter()
        .filter(|&&b| z[b].norm_sqr() > HOT_FACTOR * reference)
        .count();
    let mut acc = Complex64::new(0.0, 0.0);
    let mut mag = 0.0;
    let mut pairs = 0usize;
    for w in gap_idx.windows(2) {
        let (a, b) = (w[0], w[1]);
        if b == a + 1
            && z[a].norm_sqr() > PAIR_FACTOR * reference
            && z[b].norm_sqr() > PAIR_FACTOR * reference
        {
            let p = z[b] * z[a].conj();
            acc += p;
            mag += p.norm();
            pairs += 1;
        }
    }
    let offset_hz = (pairs > 0).then(|| acc.arg() * (fs / len as f64) / TAU);
    SecondCarrierEvidence {
        gap_blocks: gap_idx.len(),
        hot_fraction: if gap_idx.is_empty() {
            0.0
        } else {
            hot as f64 / gap_idx.len() as f64
        },
        offset_hz,
        coherence: if mag > 0.0 { acc.norm() / mag } else { 0.0 },
    }
}

/// D5: one signal below `ONE_SIGNAL_MAX_HOT`; non-CW above
/// `NON_CW_MIN_HOT`; between them, a coherent offset in
/// `[MIN_OFFSET_HZ, MAX_OFFSET_HZ]` is a keyed neighbour, anything else
/// unresolved.
pub fn classify(e: &SecondCarrierEvidence) -> Hypothesis {
    if e.hot_fraction < ONE_SIGNAL_MAX_HOT {
        return Hypothesis::OneSignal;
    }
    if e.hot_fraction > NON_CW_MIN_HOT {
        return Hypothesis::NonCw;
    }
    match e.offset_hz {
        Some(f)
            if e.coherence >= MIN_COHERENCE
                && (MIN_OFFSET_HZ..=MAX_OFFSET_HZ).contains(&f.abs()) =>
        {
            Hypothesis::TwoSignalCw { offset_hz: f }
        }
        _ => Hypothesis::Unresolved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 96_000.0;
    const A_HZ: f64 = 10_031.25;

    /// Raised-cosine keyed tone, edges centred on each element's 50 %
    /// times (seconds), f64 phase.
    fn keyed_tone(
        fs: f64,
        n: usize,
        carrier_hz: f64,
        elements: &[(f64, f64)],
        amp: f64,
        phase: f64,
    ) -> Vec<Complex32> {
        let mut out = vec![Complex32::new(0.0, 0.0); n];
        for &(s, e) in elements {
            let lo = (((s - RISE_S) * fs).floor().max(0.0) as usize).min(n);
            let hi = (((e + RISE_S) * fs).ceil().max(0.0) as usize).min(n);
            for (k, x) in out.iter_mut().enumerate().take(hi).skip(lo) {
                let t = k as f64 / fs;
                let env = rc(t - s) - rc(t - e);
                let ph = ((carrier_hz / fs) * k as f64).rem_euclid(1.0) * TAU + phase;
                *x += Complex32::new((amp * env * ph.cos()) as f32, (amp * env * ph.sin()) as f32);
            }
        }
        out
    }

    fn steady_tone(fs: f64, n: usize, carrier_hz: f64, amp: f64) -> Vec<Complex32> {
        (0..n)
            .map(|k| {
                let ph = ((carrier_hz / fs) * k as f64).rem_euclid(1.0) * TAU;
                Complex32::new((amp * ph.cos()) as f32, (amp * ph.sin()) as f32)
            })
            .collect()
    }

    /// Seeded complex Gaussian noise, per-component sigma (xorshift64 +
    /// Box-Muller; the crate has no rand dependency).
    fn awgn(n: usize, sigma: f64, seed: u64) -> Vec<Complex32> {
        let mut state = seed.max(1);
        let mut uniform = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            ((state >> 11) as f64 + 1.0) / (1u64 << 53) as f64
        };
        (0..n)
            .map(|_| {
                let r = (-2.0 * uniform().ln()).sqrt() * sigma;
                let theta = TAU * uniform();
                Complex32::new((r * theta.cos()) as f32, (r * theta.sin()) as f32)
            })
            .collect()
    }

    fn splitmix64(mut x: u64) -> u64 {
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Standard normal from two splitmix64 draws keyed on `i`.
    fn gauss(i: u64) -> f64 {
        let u1 = ((splitmix64(2 * i) >> 11) as f64 + 1.0) / (1u64 << 53) as f64;
        let u2 = ((splitmix64(2 * i + 1) >> 11) as f64) / (1u64 << 53) as f64;
        (-2.0 * u1.ln()).sqrt() * (TAU * u2).cos()
    }

    /// 25 WPM-like keying: `count` elements of `unit` or 3·`unit` with
    /// `unit`, 3·`unit` or 7·`unit` gaps, from `t0` (seconds).
    fn keying(t0: f64, unit: f64, count: usize, salt: u64) -> Vec<(f64, f64)> {
        let mut t = t0;
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let r = splitmix64(i as u64 ^ (salt << 32));
            let on = if r & 1 == 0 { unit } else { 3.0 * unit };
            let off = match (r >> 1) % 4 {
                0 | 1 => unit,
                2 => 3.0 * unit,
                _ => 7.0 * unit,
            };
            out.push((t, t + on));
            t += on + off;
        }
        out
    }

    fn end_of(els: &[(f64, f64)]) -> f64 {
        els.last().map_or(0.0, |e| e.1)
    }

    fn add(a: &mut [Complex32], b: &[Complex32]) {
        for (x, y) in a.iter_mut().zip(b) {
            *x += y;
        }
    }

    /// Amplitude of a tone `snr_db` over unit-sigma noise in 2500 Hz.
    fn amp_for_snr(snr_db: f64, fs: f64) -> f64 {
        // Unit per-component sigma: total noise power 2, 2·2500/fs in 2500 Hz.
        (10f64.powf(snr_db / 10.0) * 2.0 * 2500.0 / fs).sqrt()
    }

    /// Strong at A_HZ (+30 dB), an independent neighbour 20 dB weaker with
    /// a lower duty, unit noise. Returns the scene and the strong truth.
    fn pair_scene(neigh_off: f64, seed: u64) -> (Vec<Complex32>, Vec<(f64, f64)>) {
        let strong = keying(0.3, 0.048, 40, 1);
        let weak = keying(0.33, 0.06, 26, 2)
            .into_iter()
            .map(|(s, e)| (s, s + (e - s).min(0.06)))
            .collect::<Vec<_>>();
        let dur = end_of(&strong).max(end_of(&weak)) + 0.4;
        let n = (dur * FS) as usize;
        let mut x = keyed_tone(FS, n, A_HZ, &strong, amp_for_snr(30.0, FS), 0.3);
        add(
            &mut x,
            &keyed_tone(FS, n, A_HZ + neigh_off, &weak, amp_for_snr(10.0, FS), 1.1),
        );
        add(&mut x, &awgn(n, 1.0, seed));
        (x, strong)
    }

    fn edge_errors_ms(found: &[KeyedElement], truth: &[(f64, f64)], fs: f64) -> Vec<f64> {
        assert_eq!(found.len(), truth.len(), "element count");
        found
            .iter()
            .zip(truth)
            .flat_map(|(f, t)| [(f.start / fs - t.0) * 1e3, (f.end / fs - t.1) * 1e3])
            .collect()
    }

    fn rms(v: &[f64]) -> f64 {
        (v.iter().map(|x| x * x).sum::<f64>() / v.len() as f64).sqrt()
    }

    fn clean_scene(fs: f64, carrier: f64) -> (Vec<Complex32>, Vec<(f64, f64)>) {
        let els = keying(0.3, 0.048, 30, 1);
        let n = ((end_of(&els) + 0.4) * fs) as usize;
        (keyed_tone(fs, n, carrier, &els, 1.0, 0.7), els)
    }

    #[test]
    fn edges_land_on_the_true_half_amplitude_times() {
        let (x, truth) = clean_scene(FS, A_HZ);
        let found = find_elements(&x, FS, A_HZ);
        let err = edge_errors_ms(&found, &truth, FS);
        assert!(err.iter().all(|e| e.abs() <= 0.02), "{err:?}");
    }

    #[test]
    fn edge_window_scales_with_sample_rate() {
        for fs in [48_000.0, 192_000.0] {
            let (x, truth) = clean_scene(fs, A_HZ);
            let err = edge_errors_ms(&find_elements(&x, fs, A_HZ), &truth, fs);
            assert!(err.iter().all(|e| e.abs() <= 0.05), "{fs}: {err:?}");
        }
    }

    #[test]
    fn edges_do_not_depend_on_channel_alignment() {
        for carrier in [A_HZ, A_HZ + 46.875] {
            let (x, truth) = clean_scene(FS, carrier);
            let err = edge_errors_ms(&find_elements(&x, FS, carrier), &truth, FS);
            assert!(err.iter().all(|e| e.abs() <= 0.02), "{carrier}: {err:?}");
        }
    }

    #[test]
    fn a_twenty_db_weaker_neighbour_moves_edges_less_than_0_3_ms() {
        let (x, truth) = pair_scene(40.0, 204);
        let found = find_elements(&x, FS, A_HZ);
        let err = edge_errors_ms(&found, &truth, FS);
        assert!(rms(&err) <= 0.30, "edge rms {} ms", rms(&err));
        assert_eq!(found, find_elements(&x, FS, A_HZ), "deterministic");
    }

    #[test]
    fn refinement_corrects_a_three_hertz_hint() {
        let (x, _) = pair_scene(40.0, 204);
        let est = estimate_strong(&x, FS, A_HZ + 3.0);
        assert!((est.carrier_hz - A_HZ).abs() <= 0.1, "{}", est.carrier_hz);
        assert!(
            (est.carrier_step_hz + 3.0).abs() <= 0.2,
            "{}",
            est.carrier_step_hz
        );
        assert!(est.fits.len() >= MIN_ELEMENTS);
    }

    fn element_energy(x: &[Complex32], fs: f64, els: &[(f64, f64)]) -> f64 {
        els.iter()
            .flat_map(|&(s, e)| {
                let lo = ((s - RISE_S) * fs).max(0.0) as usize;
                let hi = (((e + RISE_S) * fs) as usize).min(x.len());
                x[lo..hi].iter().map(|v| v.norm_sqr() as f64)
            })
            .sum()
    }

    #[test]
    fn subtraction_leaves_less_than_minus_40_db_of_a_clean_tone() {
        let (x, els) = clean_scene(FS, A_HZ);
        let est = estimate_strong(&x, FS, A_HZ + 0.3);
        let mut r = x.clone();
        subtract(&mut r, FS, &est);
        let db = 10.0 * (element_energy(&r, FS, &els) / element_energy(&x, FS, &els)).log10();
        assert!(db <= -40.0, "{db} dB");
    }

    #[test]
    fn subtraction_keeps_phase_precision_far_from_dc() {
        let fs = 192_000.0;
        let carrier = 45_000.0;
        let els: Vec<(f64, f64)> = keying(0.3, 0.048, 400, 3)
            .into_iter()
            .take_while(|e| e.1 < 59.6)
            .collect();
        let n = (60.0 * fs) as usize;
        let x = keyed_tone(fs, n, carrier, &els, 1.0, 0.2);
        let est = estimate_strong(&x, fs, carrier);
        let mut r = x.clone();
        subtract(&mut r, fs, &est);
        let first: Vec<_> = els.iter().copied().filter(|e| e.1 < 1.3).collect();
        let last: Vec<_> = els.iter().copied().filter(|e| e.0 > 58.6).collect();
        for part in [first, last] {
            let db = 10.0 * (element_energy(&r, fs, &part) / element_energy(&x, fs, &part)).log10();
            assert!(db <= -40.0, "{db} dB over {part:?}");
        }
    }

    #[test]
    fn elements_cut_by_the_buffer_rims_do_not_panic() {
        let mut els = keying(0.3, 0.048, 10, 1);
        els.insert(0, (-0.05, 0.1));
        let last = end_of(&els);
        els.push((last + 0.1, last + 0.5));
        let n = ((last + 0.2) * FS) as usize;
        let x = keyed_tone(FS, n, A_HZ, &els, 1.0, 0.0);
        let found = find_elements(&x, FS, A_HZ);
        assert_eq!(found.len(), 10, "only complete elements");
        let mut with_short = found.clone();
        with_short.push(KeyedElement {
            start: 0.0,
            end: 2.0 * RISE_S * FS + 1.0,
        });
        with_short.push(KeyedElement {
            start: n as f64 - 100.0,
            end: n as f64 + 5_000.0,
        });
        let fits = fit_elements(&x, FS, A_HZ, &with_short);
        assert_eq!(fits.len(), 10);
        let est = StrongEstimate {
            carrier_hz: A_HZ,
            carrier_step_hz: 0.0,
            fits,
        };
        let mut r = x.clone();
        subtract(&mut r, FS, &est);
        let _ = residual_check(&x, FS, &est);
        let _ = second_carrier_evidence(&x, FS, &est);
    }

    #[test]
    fn an_unstable_refinement_is_reported() {
        let x = awgn((10.0 * 48_000.0) as usize, 1.0, 9);
        let est = estimate_strong(&x, 48_000.0, 5_000.0);
        assert!(
            est.fits.len() < MIN_ELEMENTS || est.carrier_step_hz.abs() > MAX_CARRIER_STEP_HZ,
            "{} fits, step {}",
            est.fits.len(),
            est.carrier_step_hz
        );
    }

    fn shifted(est: &StrongEstimate, by_ms: f64) -> StrongEstimate {
        let mut e = est.clone();
        for f in &mut e.fits {
            f.element.start += by_ms * 1e-3 * FS;
            f.element.end += by_ms * 1e-3 * FS;
        }
        e
    }

    #[test]
    fn a_clean_cancellation_passes_the_check() {
        let (x, _) = pair_scene(40.0, 204);
        let est = estimate_strong(&x, FS, A_HZ + 0.3);
        let check = residual_check(&x, FS, &est);
        assert!(check.edge_over_neighbour_db <= -6.0, "{check:?}");
        assert!(check.interior_over_neighbour_db <= -10.0, "{check:?}");
        assert!(check.clean());
    }

    #[test]
    fn a_three_hertz_carrier_error_fails_the_check() {
        let (x, _) = pair_scene(40.0, 204);
        let wrong = A_HZ + 3.0;
        let fits = fit_elements(&x, FS, wrong, &find_elements(&x, FS, wrong));
        let est = StrongEstimate {
            carrier_hz: wrong,
            carrier_step_hz: 0.0,
            fits: fits
                .into_iter()
                .map(|mut f| {
                    f.slope_rad_per_sample = 0.0;
                    f
                })
                .collect(),
        };
        let check = residual_check(&x, FS, &est);
        assert!(check.interior_over_neighbour_db > -4.0, "{check:?}");
        assert!(!check.clean());
    }

    #[test]
    fn shifted_edges_fail_the_edge_ratio() {
        let (x, _) = pair_scene(40.0, 204);
        let est = estimate_strong(&x, FS, A_HZ + 0.3);
        for ms in [0.67, 1.33] {
            let check = residual_check(&x, FS, &shifted(&est, ms));
            assert!(check.edge_over_neighbour_db > -5.0, "{ms}: {check:?}");
            assert!(!check.clean());
        }
    }

    #[test]
    fn heavily_jittered_edges_fail_the_edge_ratio() {
        let (x, _) = pair_scene(40.0, 204);
        let mut est = estimate_strong(&x, FS, A_HZ + 0.3);
        for (i, f) in est.fits.iter_mut().enumerate() {
            f.element.start += 0.8e-3 * FS * gauss(2 * i as u64);
            f.element.end += 0.8e-3 * FS * gauss(2 * i as u64 + 1);
        }
        let check = residual_check(&x, FS, &est);
        assert!(!check.clean(), "{check:?}");
    }

    #[test]
    fn an_unremoved_rim_element_does_not_flip_a_clean_verdict() {
        let (mut x, _) = pair_scene(40.0, 204);
        let est = estimate_strong(&x, FS, A_HZ + 0.3);
        let extra = keyed_tone(
            FS,
            x.len(),
            A_HZ,
            &[(0.02, 0.14)],
            amp_for_snr(30.0, FS),
            0.0,
        );
        add(&mut x, &extra);
        let check = residual_check(&x, FS, &est);
        assert!(check.clean(), "{check:?}");
    }

    #[test]
    fn a_lone_strong_station_has_no_second_carrier() {
        let strong = keying(0.3, 0.048, 40, 1);
        let n = ((end_of(&strong) + 0.4) * FS) as usize;
        let mut x = keyed_tone(FS, n, A_HZ, &strong, amp_for_snr(30.0, FS), 0.3);
        add(&mut x, &awgn(n, 1.0, 204));
        let est = estimate_strong(&x, FS, A_HZ + 0.3);
        let ev = second_carrier_evidence(&x, FS, &est);
        assert!(ev.hot_fraction < 0.10, "{ev:?}");
        assert_eq!(classify(&ev), Hypothesis::OneSignal);
    }

    #[test]
    fn a_keyed_neighbour_is_two_signal_cw_at_its_offset() {
        for off in [40.0, -40.0, 120.0] {
            let (x, _) = pair_scene(off, 204);
            let est = estimate_strong(&x, FS, A_HZ + 0.3);
            let ev = second_carrier_evidence(&x, FS, &est);
            match classify(&ev) {
                Hypothesis::TwoSignalCw { offset_hz } => {
                    assert!((offset_hz - off).abs() <= 1.0, "{off}: {ev:?}")
                }
                h => panic!("{off}: {h:?} from {ev:?}"),
            }
            assert!(ev.coherence >= 0.9, "{off}: {ev:?}");
        }
    }

    #[test]
    fn a_steady_neighbour_is_non_cw() {
        let strong = keying(0.3, 0.048, 40, 1);
        let n = ((end_of(&strong) + 0.4) * FS) as usize;
        let mut x = keyed_tone(FS, n, A_HZ, &strong, amp_for_snr(30.0, FS), 0.3);
        add(
            &mut x,
            &steady_tone(FS, n, A_HZ + 40.0, amp_for_snr(10.0, FS)),
        );
        add(&mut x, &awgn(n, 1.0, 204));
        let est = estimate_strong(&x, FS, A_HZ + 0.3);
        let ev = second_carrier_evidence(&x, FS, &est);
        assert!(ev.hot_fraction > 0.9, "{ev:?}");
        assert_eq!(classify(&ev), Hypothesis::NonCw);
    }

    #[test]
    fn classify_boundaries() {
        let ev = |h: f64, c: f64, f: Option<f64>| SecondCarrierEvidence {
            gap_blocks: 100,
            hot_fraction: h,
            offset_hz: f,
            coherence: c,
        };
        let two = |f: f64| Hypothesis::TwoSignalCw { offset_hz: f };
        let cases = [
            (ev(0.14, 0.9, Some(40.0)), Hypothesis::OneSignal),
            (ev(0.15, 0.9, Some(40.0)), two(40.0)),
            (ev(0.75, 0.9, Some(40.0)), two(40.0)),
            (ev(0.76, 0.9, Some(40.0)), Hypothesis::NonCw),
            (ev(0.4, 0.79, Some(40.0)), Hypothesis::Unresolved),
            (ev(0.4, 0.80, Some(40.0)), two(40.0)),
            (ev(0.4, 0.9, Some(9.9)), Hypothesis::Unresolved),
            (ev(0.4, 0.9, Some(-10.0)), two(-10.0)),
            (ev(0.4, 0.9, Some(150.0)), two(150.0)),
            (ev(0.4, 0.9, Some(150.1)), Hypothesis::Unresolved),
            (ev(0.4, 0.9, None), Hypothesis::Unresolved),
            (ev(0.14, 0.0, None), Hypothesis::OneSignal),
            (ev(0.9, 0.0, None), Hypothesis::NonCw),
        ];
        for (e, want) in cases {
            assert_eq!(classify(&e), want, "{e:?}");
        }
    }

    #[test]
    fn a_noiseless_scene_has_a_finite_reference() {
        let (x, _) = clean_scene(FS, A_HZ);
        let est = estimate_strong(&x, FS, A_HZ);
        let ev = second_carrier_evidence(&x, FS, &est);
        assert!(
            ev.hot_fraction.is_finite() && ev.coherence.is_finite(),
            "{ev:?}"
        );
        assert_eq!(classify(&ev), Hypothesis::OneSignal);
        let check = residual_check(&x, FS, &est);
        assert!(check.edge_over_neighbour_db.is_finite(), "{check:?}");
    }
}
