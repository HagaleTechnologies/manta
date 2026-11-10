//! WOLA polyphase filterbank channelizer (SPEC §1.1-1.3): the full
//! N-channel successor to the M0 single-channel shim (`single.rs`).

use crate::condition::{ConditioningConfig, ConditioningLog, InputConditioner};
use crate::proto::{design_prototype, TAPS_PER_BRANCH};
use crate::sample_guard;
use num_complex::{Complex32, Complex64};
use std::sync::Arc;

pub const CHANNEL_SPACING_HZ: f64 = 93.75; // SPEC §1.1
/// SPEC §1.3/§1.4 power-to-dB epsilon.
const POWER_DB_EPSILON: f64 = 1e-20;

/// One hop's channelizer output: per-channel complex spectrum and power.
/// SPEC §1.3.
#[derive(Debug, Clone)]
pub struct HopOutput {
    /// Hop index, monotonically increasing from stream start.
    pub m: u64,
    /// Complex per-channel spectrum, FFT bin order (index k; SPEC §1.1's
    /// f(k) mapping applies via `Channelizer::channel_freq_hz`).
    pub x: Vec<Complex32>,
    /// Per-channel power, `|X[k]|^2`. SPEC §1.3 step 4.
    pub power: Vec<f32>,
}

/// `PdB = 10*log10(P + epsilon)`, SPEC §1.3/§1.4's epsilon = 1e-20.
pub fn power_db(power: f32) -> f64 {
    10.0 * (power as f64 + POWER_DB_EPSILON).log10()
}

/// SPEC §1.3 step 2's circular rotation (`r = (m*hop) mod N`, `hop = N/4`)
/// leaves a residual checkerboard sign artifact on `HopOutput::x[k]`: odd
/// channels at odd hops come out negated relative to what a phase-
/// continuous baseband signal would read, `(-1)^(m*k)`. Magnitude-only
/// consumers (`power = |X[k]|^2`, everything before MAN-168) are
/// invariant to this and never needed to correct it. A consumer working
/// with the complex spectrum directly (MAN-168's narrowband refiner,
/// which derotates and lowpass-filters `x[k]` and so needs a genuinely
/// phase-continuous input) must multiply `x[k]` by this factor first.
///
/// Found by Codex review on PR #161's MAN-168 wiring (a probe using the
/// real channelizer + refiner measured ~39 dB of spurious attenuation on
/// one specific odd channel). The exact form here was verified
/// empirically, not hand-derived from the rotation step's DFT shift
/// theorem (an initial `exp(-j*2*pi*k*r/N)` derivation, mathematically
/// plausible for an abstract circular shift, measured identical -- and
/// still wrong -- results for both candidate signs; the real artifact
/// turned out to be this much simpler period-2 real sign flip). Confirmed
/// against both 96 kHz/1024-channel and 192 kHz/2048-channel
/// configurations, multiple even/odd channel pairs, and both a centered
/// and a fractionally-offset tone: after this correction, an odd
/// channel's refined output exactly matches an even channel's.
pub fn odd_channel_sign_correction(hop_m: u64, k: usize) -> f32 {
    if hop_m % 2 == 1 && k % 2 == 1 {
        -1.0
    } else {
        1.0
    }
}

/// Per-hop fine-frequency interpolation (SPEC §1.4): quadratic
/// interpolation on dB powers of the three bins around a candidate peak
/// channel. Returns the sub-bin offset in `[-0.5, 0.5]`, or `None` if the
/// hop is "unusable" (no local maximum at the center bin).
pub fn interpolate_offset(p_minus: f32, p_zero: f32, p_plus: f32) -> Option<f64> {
    let pm = power_db(p_minus);
    let p0 = power_db(p_zero);
    let pp = power_db(p_plus);
    let denom = pm - 2.0 * p0 + pp;
    if denom < 0.0 {
        Some((0.5 * (pm - pp) / denom).clamp(-0.5, 0.5))
    } else {
        None
    }
}

/// hop = N/4 (SPEC §1.1): a 2π phase advance per hop spans 4 channels on
/// every channel table.
pub const CHANNELS_PER_PHASE_CYCLE: f64 = 4.0;

/// Lag-1 phase product of channel `k` between consecutive hops (SPEC §1.4,
/// MAN-114), rotated into the absolute frame so products from different
/// channels of one track can be averaged: its argument is
/// `2π·p/4 (mod 2π)` for a tone at channel position `p`. Both hops get
/// `odd_channel_sign_correction`. `None` unless `cur.m == prev.m + 1`, both
/// spectra are `n` long (power-only hops carry an empty `x`) and `k < n`.
///
/// The rotation `e^{jπ(k mod 4)/2}` undoes channel `k`'s downconversion:
/// the corrected output of channel `k` advances `2π·(p − k)/4` per hop.
/// It holds for upper-half (negative-frequency) channels too, because N
/// is a multiple of 4.
pub fn phase_lag_product(prev: &HopOutput, cur: &HopOutput, k: usize) -> Option<Complex64> {
    let n = cur.power.len();
    if prev.m.checked_add(1) != Some(cur.m) || cur.x.len() != n || prev.x.len() != n || k >= n {
        return None;
    }
    let a = cur.x[k] * odd_channel_sign_correction(cur.m, k);
    let b = prev.x[k] * odd_channel_sign_correction(prev.m, k);
    let z = a * b.conj();
    let z = Complex64::new(z.re as f64, z.im as f64);
    Some(z * Complex64::from_polar(1.0, std::f64::consts::FRAC_PI_2 * (k % 4) as f64))
}

/// Channel position of an absolute-frame phase sum (`phase_lag_product`
/// accumulated): `arg(z)/2π·4`, unwrapped to the value nearest `near`
/// (a track's `center`). `None` for a zero sum. SPEC §1.4.
pub fn phase_position(z: Complex64, near: f64) -> Option<f64> {
    if z.re == 0.0 && z.im == 0.0 {
        return None;
    }
    let p = z.arg() / std::f64::consts::TAU * CHANNELS_PER_PHASE_CYCLE;
    Some(p + CHANNELS_PER_PHASE_CYCLE * ((near - p) / CHANNELS_PER_PHASE_CYCLE).round())
}

/// WOLA polyphase filterbank: N channels, 4x oversampled (hop = N/4).
/// SPEC §1.1-1.3.
pub struct Channelizer {
    n: usize,
    hop: usize,
    /// SPEC §1.2's prototype reversed, `taps_rev[i] = h[LN-1-i]`, so slab p
    /// of the window meets `taps_rev[p*N..(p+1)*N]` in order (MAN-527).
    /// Length L*N, L = TAPS_PER_BRANCH.
    taps_rev: Vec<f32>,
    /// MAN-117: the forward plan `coppa_dsp::fft::FftProcessor::new(n)`
    /// builds (`FftPlanner::new().plan_fft_forward(n)`), run in place with
    /// `fft_scratch` so a hop allocates only its own `x` and `power`.
    fwd: Arc<dyn rustfft::Fft<f32>>,
    /// MAN-117: `fwd.get_inplace_scratch_len()` elements, reused every hop.
    fft_scratch: Vec<Complex32>,
    /// MAN-117: SPEC §1.3 step 1's N-point f64 fold, reused every hop.
    fold: Vec<Complex64>,
    /// MAN-527: hops `process_into` recycled from the caller's previous
    /// `out`, reused before anything is allocated; at most
    /// `SPARE_RETAIN_HOPS` kept after a call.
    spare: Vec<HopOutput>,
    /// Sliding input window; `read` indexes the next window start. Samples
    /// before `read` are dead and get compacted away (same pattern as
    /// `single.rs::SingleChannelExtractor`).
    buf: Vec<Complex32>,
    read: usize,
    /// Hop output counter, used for the SPEC §1.3 step-2 rotation `r =
    /// (m*hop) mod N`. A plain integer counter, not an accumulated phase --
    /// no drift/precision concern (unlike an NCO), so no need to derive it
    /// from an absolute sample index.
    m: u64,
    fs: f64,
    center_freq_hz: f64,
    /// MAN-234: input samples `process` replaced with silence since
    /// construction.
    bad_samples: u64,
    /// MAN-203: the optional input-conditioning chain, `None` when every
    /// stage is off.
    conditioner: Option<InputConditioner>,
    /// MAN-203: `buf[..ready]` is final (conditioned); the rest is held
    /// back by an applying impulse stage's lookahead. Windows are formed
    /// only from final samples.
    ready: usize,
}

impl Channelizer {
    /// A channelizer for a supported table rate (`fs/93.75` a power of
    /// two), with no input conditioning. SPEC §1.1.
    pub fn new(fs: f64, center_freq_hz: f64) -> Result<Self, String> {
        Self::with_conditioning(fs, center_freq_hz, &ConditioningConfig::default())
    }

    /// [`Channelizer::new`] plus the MAN-203 input-conditioning chain
    /// `cfg`, run on every input sample right after the MAN-234 sample
    /// guard (SPEC §1.3 step 0a). With every stage off this is exactly
    /// `new`: no conditioner is built and no sample is touched.
    pub fn with_conditioning(
        fs: f64,
        center_freq_hz: f64,
        cfg: &ConditioningConfig,
    ) -> Result<Self, String> {
        let nf = fs / CHANNEL_SPACING_HZ;
        let n = nf.round() as usize;
        if (nf - n as f64).abs() > 1e-9 || !n.is_power_of_two() {
            return Err(format!(
                "unsupported sample rate {fs}: fs/93.75 must be a power of two"
            ));
        }
        let fwd = rustfft::FftPlanner::<f32>::new().plan_fft_forward(n);
        let fft_scratch = vec![Complex32::new(0.0, 0.0); fwd.get_inplace_scratch_len()];
        Ok(Channelizer {
            n,
            hop: n / 4,
            taps_rev: {
                let mut taps = design_prototype(n, TAPS_PER_BRANCH);
                taps.reverse();
                taps
            },
            fwd,
            fft_scratch,
            fold: vec![Complex64::new(0.0, 0.0); n],
            spare: Vec::new(),
            buf: Vec::new(),
            read: 0,
            m: 0,
            fs,
            center_freq_hz,
            bad_samples: 0,
            conditioner: InputConditioner::with_geometry(fs, center_freq_hz, n, cfg),
            ready: 0,
        })
    }

    /// What the input-conditioning chain did since construction, or `None`
    /// when every stage is off (MAN-203).
    pub fn conditioning(&self) -> Option<&ConditioningLog> {
        self.conditioner.as_ref().map(InputConditioner::log)
    }

    /// Number of channels, N. SPEC §1.1.
    pub fn n_channels(&self) -> usize {
        self.n
    }

    /// Input samples consumed per output hop (N/4). SPEC §1.1.
    pub fn hop(&self) -> usize {
        self.hop
    }

    /// Prototype filter length in taps (L*N). Same causal-filter blind-zone
    /// property as `single.rs`'s extractor -- see the M0 lead-in-padding
    /// fix in `manta-engine`, which Task 6/7 apply here too.
    pub fn filter_len(&self) -> usize {
        self.taps_rev.len()
    }

    /// Channel `k`'s RF center frequency. SPEC §1.1:
    /// `f(k) = f_center + ((k + N/2) mod N - N/2) * Delta`.
    pub fn channel_freq_hz(&self, k: usize) -> f64 {
        let delta = self.fs / self.n as f64;
        let signed = ((k + self.n / 2) % self.n) as f64 - (self.n / 2) as f64;
        self.center_freq_hz + signed * delta
    }

    /// Input samples `process` has replaced with silence since construction
    /// because a component was NaN, infinite or beyond
    /// `sample_guard::MAX_SAMPLE_MAGNITUDE`. SPEC §1.3 step 0 (MAN-234).
    pub fn bad_samples(&self) -> u64 {
        self.bad_samples
    }

    /// Feed input IQ; returns however many hops became available. SPEC §1.3.
    /// Step 0 (MAN-234): an unusable sample (`sample_guard::is_usable`) is
    /// replaced with `0+0j` as it enters the window and counted in
    /// `bad_samples`; usable samples are never written. Step 0a (MAN-203):
    /// the optional conditioning chain then runs on the new samples, in
    /// stream order, so the hops do not depend on how the input is split.
    /// An applying impulse stage holds back its 21-sample lookahead, so
    /// windows are formed only up to the last final sample: sample indices
    /// do not move, and at most the stream's final `ceil(21 / hop)` hops
    /// are never formed (one hop at every rate from 12 kS/s up, where
    /// `hop >= 32`).
    pub fn process(&mut self, iq: &[Complex32]) -> Vec<HopOutput> {
        let mut out = Vec::new();
        self.process_into(iq, &mut out);
        out
    }

    /// [`Channelizer::process`], writing the hops into `out` instead of a
    /// new `Vec`. `out`'s previous contents are recycled, not kept: on
    /// return it holds exactly the hops this call produced. Any
    /// `HopOutput` may be passed in; its buffers are reused when their
    /// capacity is enough. With one `out` kept across calls, a steady-state
    /// call allocates nothing (MAN-527).
    pub fn process_into(&mut self, iq: &[Complex32], out: &mut Vec<HopOutput>) {
        self.spare.append(out);
        let start = self.buf.len();
        self.buf.extend_from_slice(iq);
        self.bad_samples += sample_guard::replace_unusable(&mut self.buf[start..]);
        let ready = self.ready;
        self.ready = match self.conditioner.as_mut() {
            Some(c) => ready + c.condition(&mut self.buf[ready..]),
            None => self.buf.len(),
        };
        let n = self.n;
        let ln = self.taps_rev.len();
        while self.read + ln <= self.ready {
            let window = &self.buf[self.read..self.read + ln];

            // Steps 1-2: window, fold and rotate left by r = (m*hop) mod N.
            // u[n] = x[n]*h[LN-1-n]; v[j] = sum_p u[j + p*N], summed slab by
            // slab (p ascending, channels inner, contiguous slices) in f64
            // from +0.0 into the reused `fold` buffer (SPEC §6.4, MAN-117),
            // with the rotation applied as the write index (MAN-527).
            let r = ((self.m.wrapping_mul(self.hop as u64)) % n as u64) as usize;
            fold_rotated(&mut self.fold, window, &self.taps_rev, r);

            // The hop's buffers come from the spare pool when it has one
            // (MAN-527); a fresh hop allocates exactly its `x` and `power`.
            let mut hop = self.spare.pop().unwrap_or_else(|| HopOutput {
                m: 0,
                x: Vec::with_capacity(n),
                power: Vec::with_capacity(n),
            });
            hop.m = self.m;
            // Narrow to f32: the same values as rotating the f32 copy.
            hop.x.clear();
            hop.x.extend(
                self.fold
                    .iter()
                    .map(|c| Complex32::new(c.re as f32, c.im as f32)),
            );

            // Step 3: FFT, in place on the hop's own output buffer with the
            // channelizer's scratch; bit-identical to `FftProcessor::forward`
            // (MAN-117). Step 4: power.
            self.fwd
                .process_with_scratch(&mut hop.x, &mut self.fft_scratch);
            hop.power.clear();
            hop.power.extend(hop.x.iter().map(|c| c.norm_sqr()));
            if let Some(c) = self.conditioner.as_mut() {
                c.observe_hop(&hop.x, self.m * self.hop as u64);
            }
            out.push(hop);

            self.m += 1;
            self.read += self.hop;
        }
        self.spare.truncate(SPARE_RETAIN_HOPS);
        if self.read >= ln {
            self.buf.drain(..self.read);
            self.ready -= self.read;
            self.read = 0;
        }
    }
}

/// MAN-527: recycled hops `process_into` keeps beyond what a call used. 32
/// covers the lead-in padding call (`filter_len / hop` = 32 hops) and four
/// offline 8-hop batches; a larger one-off read's surplus is released. At
/// N = 2048 a hop's buffers are 24 KB, so the pool holds at most 768 KB.
const SPARE_RETAIN_HOPS: usize = 32;

/// SPEC §1.3 steps 1-2 for one hop, into `fold` (length N): window and fold,
/// then rotate left by `r`, with the rotation applied as the write index.
/// Slab p of `window` (samples p*N..(p+1)*N) meets `taps_rev[p*N..(p+1)*N]`;
/// slab position j lands at `fold[(j + N - r) mod N]`, so positions r..N fill
/// `fold[..N-r]` and positions 0..r fill `fold[N-r..]`. Every `fold[j]`
/// starts at +0.0 and adds its products in ascending p, as separate f64
/// multiplies and adds (SPEC §6 rule 4), so the values are bit-identical to
/// the per-sample loop this replaces (MAN-527).
fn fold_rotated(fold: &mut [Complex64], window: &[Complex32], taps_rev: &[f32], r: usize) {
    let n = fold.len();
    fold.fill(Complex64::new(0.0, 0.0));
    for (w, h) in window.chunks_exact(n).zip(taps_rev.chunks_exact(n)) {
        let (w_lo, w_hi) = w.split_at(r);
        let (h_lo, h_hi) = h.split_at(r);
        let (f_lo, f_hi) = fold.split_at_mut(n - r);
        accumulate(f_lo, w_hi, h_hi);
        accumulate(f_hi, w_lo, h_lo);
    }
}

/// `acc[j] += h[j] * w[j]` in f64 over three equal-length slices. A separate
/// multiply and add, never `mul_add`: an FMA would change the bits (MAN-527).
fn accumulate(acc: &mut [Complex64], w: &[Complex32], h: &[f32]) {
    for ((a, x), &h) in acc.iter_mut().zip(w).zip(h) {
        let h = h as f64;
        a.re += h * x.re as f64;
        a.im += h * x.im as f64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coppa_dsp::fft::FftProcessor;

    /// Deterministic LCG pseudo-noise plus two tones (MAN-117 fixtures).
    fn noise_and_tones(fs: f64, n: usize) -> Vec<Complex32> {
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((state >> 40) as f32 / (1u64 << 24) as f32) - 0.5
        };
        (0..n)
            .map(|i| {
                let t = i as f64 / fs;
                let a = 2.0 * std::f64::consts::PI * 1_234.5 * t;
                let b = 2.0 * std::f64::consts::PI * -20_007.0 * t;
                let tones = Complex32::new(
                    (0.3 * a.cos() + 0.05 * b.cos()) as f32,
                    (0.3 * a.sin() + 0.05 * b.sin()) as f32,
                );
                tones + Complex32::new(0.1 * next(), 0.1 * next())
            })
            .collect()
    }

    /// MAN-117: the pre-MAN-117 per-hop path, verbatim: fold into a fresh
    /// `Vec<Complex64>`, convert to f32, rotate into a fresh `Vec`, coppa
    /// `FftProcessor::forward`, power.
    fn reference_hops(fs: f64, iq: &[Complex32]) -> Vec<(Vec<Complex32>, Vec<f32>)> {
        let n = (fs / CHANNEL_SPACING_HZ).round() as usize;
        let hop = n / 4;
        let taps = design_prototype(n, TAPS_PER_BRANCH);
        let ln = taps.len();
        let fft = FftProcessor::new(n);
        let mut out = Vec::new();
        let mut read = 0usize;
        let mut m = 0u64;
        while read + ln <= iq.len() {
            let window = &iq[read..read + ln];
            let mut v = vec![Complex64::new(0.0, 0.0); n];
            for (n_idx, &x) in window.iter().enumerate() {
                let h = taps[ln - 1 - n_idx] as f64;
                let j = n_idx % n;
                v[j].re += h * x.re as f64;
                v[j].im += h * x.im as f64;
            }
            let v: Vec<Complex32> = v
                .iter()
                .map(|c| Complex32::new(c.re as f32, c.im as f32))
                .collect();
            let r = ((m.wrapping_mul(hop as u64)) % n as u64) as usize;
            let mut v_rot = vec![Complex32::new(0.0, 0.0); n];
            for j in 0..n {
                v_rot[j] = v[(j + r) % n];
            }
            let x = fft.forward(&v_rot);
            let power: Vec<f32> = x.iter().map(|c| c.norm_sqr()).collect();
            out.push((x, power));
            m += 1;
            read += hop;
        }
        out
    }

    #[test]
    fn process_is_bit_identical_to_the_fft_processor_path() {
        for fs in [96_000.0, 192_000.0] {
            let n = (fs / CHANNEL_SPACING_HZ).round() as usize;
            let hops = 400usize;
            let len = TAPS_PER_BRANCH * n + (hops - 1) * (n / 4);
            let iq = noise_and_tones(fs, len);
            let reference = reference_hops(fs, &iq);
            assert_eq!(reference.len(), hops);
            for chunk in [4096usize, 1000] {
                let mut ch = Channelizer::new(fs, 14e6).unwrap();
                let got: Vec<HopOutput> = iq.chunks(chunk).flat_map(|c| ch.process(c)).collect();
                assert_eq!(got.len(), reference.len(), "fs {fs} chunk {chunk}");
                for (i, (h, (x, power))) in got.iter().zip(&reference).enumerate() {
                    assert_eq!(h.m, i as u64);
                    let bits = |v: &[Complex32]| -> Vec<(u32, u32)> {
                        v.iter().map(|c| (c.re.to_bits(), c.im.to_bits())).collect()
                    };
                    assert_eq!(bits(&h.x), bits(x), "x, fs {fs} chunk {chunk} hop {i}");
                    let pbits = |v: &[f32]| -> Vec<u32> { v.iter().map(|p| p.to_bits()).collect() };
                    assert_eq!(
                        pbits(&h.power),
                        pbits(power),
                        "power, fs {fs} chunk {chunk} hop {i}"
                    );
                }
            }
        }
    }

    #[test]
    fn in_place_forward_matches_fft_processor_forward() {
        for n in [64usize, 1024, 2048] {
            let input = noise_and_tones(96_000.0, n);
            let fwd = rustfft::FftPlanner::<f32>::new().plan_fft_forward(n);
            let mut scratch = vec![Complex32::new(0.0, 0.0); fwd.get_inplace_scratch_len()];
            let mut buf = input.clone();
            fwd.process_with_scratch(&mut buf, &mut scratch);
            let expected = FftProcessor::new(n).forward(&input);
            for (k, (a, b)) in buf.iter().zip(&expected).enumerate() {
                assert_eq!(
                    (a.re.to_bits(), a.im.to_bits()),
                    (b.re.to_bits(), b.im.to_bits()),
                    "n {n} bin {k}"
                );
            }
        }
    }

    /// MAN-527: the old fold and rotation, verbatim, in f64: u[n] = x[n] *
    /// h[LN-1-n] accumulated into v[n % N] in ascending n (so ascending p per
    /// j) from +0.0, then v'[j] = v[(j + r) % N].
    fn reference_fold(window: &[Complex32], taps: &[f32], n: usize, r: usize) -> Vec<Complex64> {
        let ln = taps.len();
        let mut v = vec![Complex64::new(0.0, 0.0); n];
        for (n_idx, &x) in window.iter().enumerate() {
            let h = taps[ln - 1 - n_idx] as f64;
            let j = n_idx % n;
            v[j].re += h * x.re as f64;
            v[j].im += h * x.im as f64;
        }
        (0..n).map(|j| v[(j + r) % n]).collect()
    }

    /// MAN-527: `fold_rotated` gives the old per-sample fold's f64 values bit
    /// for bit, so it sums each channel in the same order (p ascending).
    /// Output-level identity cannot show this: reversing the slab order still
    /// passed `process_is_bit_identical_to_the_fft_processor_path`, because
    /// narrowing to f32 absorbed the difference, while this test failed with
    /// 10,207 of 16,384 f64 values different at 96 kS/s.
    #[test]
    fn fold_rotated_matches_the_spec_sum_bit_for_bit_in_f64() {
        for fs in [96_000.0, 192_000.0] {
            let n = (fs / CHANNEL_SPACING_HZ).round() as usize;
            let hop = n / 4;
            let taps = design_prototype(n, TAPS_PER_BRANCH);
            let mut taps_rev = taps.clone();
            taps_rev.reverse();
            let ln = taps.len();
            let mut fold = vec![Complex64::new(0.0, 0.0); n];
            let mut check = |label: &str, window: &[Complex32], r: usize| {
                fold_rotated(&mut fold, window, &taps_rev, r);
                let expected = reference_fold(window, &taps, n, r);
                let differing = fold
                    .iter()
                    .zip(&expected)
                    .filter(|(a, b)| {
                        (a.re.to_bits(), a.im.to_bits()) != (b.re.to_bits(), b.im.to_bits())
                    })
                    .count();
                assert_eq!(differing, 0, "fs {fs} window {label} r {r}");
            };
            // 16 consecutive hops, so each r in {0, N/4, N/2, 3N/4} appears 4 times.
            let iq = noise_and_tones(fs, ln + 15 * hop);
            for m in 0..16usize {
                let r = (m * hop) % n;
                check(&format!("hop {m}"), &iq[m * hop..m * hop + ln], r);
            }
            let silent = vec![Complex32::new(-0.0, -0.0); ln];
            let mut half = noise_and_tones(fs, ln);
            for c in &mut half[ln / 2..] {
                *c = Complex32::new(0.0, -0.0);
            }
            for r in [0, hop, 2 * hop, 3 * hop] {
                check("all -0.0", &silent, r);
                check("noise then signed zeros", &half, r);
            }
        }
    }

    #[test]
    fn process_into_with_recycled_buffers_matches_process() {
        let check =
            |label: &str, make: &dyn Fn() -> Channelizer, iq: &[Complex32], chunk: usize| {
                let reference = make().process(iq);
                let mut ch = make();
                let n = ch.n_channels();
                let mut out = Vec::new();
                let mut collected = Vec::new();
                for piece in iq.chunks(chunk) {
                    ch.process_into(piece, &mut out);
                    collected.extend(out.iter().cloned());
                }
                assert!(!reference.is_empty(), "{label} chunk {chunk}");
                assert_hops_bitwise_equal(&reference, &collected);
                for h in &collected {
                    assert_eq!(h.x.len(), n, "{label} chunk {chunk} hop {}", h.m);
                    assert_eq!(h.power.len(), n, "{label} chunk {chunk} hop {}", h.m);
                }
            };
        for fs in [96_000.0, 192_000.0] {
            let n = (fs / CHANNEL_SPACING_HZ).round() as usize;
            let iq = noise_and_tones(fs, TAPS_PER_BRANCH * n + 199 * (n / 4));
            for chunk in [1usize, 7, 1000, 4096] {
                if chunk == 1 && fs != 96_000.0 {
                    continue;
                }
                check(
                    &format!("fs {fs}"),
                    &|| Channelizer::new(fs, 14e6).unwrap(),
                    &iq,
                    chunk,
                );
            }
        }
        use crate::condition::StageMode;
        let cfg = ConditioningConfig {
            dc: StageMode::Apply,
            impulse: StageMode::Apply,
            ..Default::default()
        };
        let iq = tone_with_impulses(30_000);
        for chunk in [7usize, 1000, 4096] {
            check(
                "conditioned",
                &|| Channelizer::with_conditioning(FS, 0.0, &cfg).unwrap(),
                &iq,
                chunk,
            );
        }
    }

    #[test]
    fn process_into_overwrites_whatever_out_held() {
        let fs = 96_000.0;
        let n = (fs / CHANNEL_SPACING_HZ).round() as usize;
        let iq = noise_and_tones(fs, TAPS_PER_BRANCH * n + 7 * (n / 4));
        let junk = |x_len: usize| HopOutput {
            m: 99,
            x: vec![Complex32::new(3.0, -3.0); x_len],
            power: vec![1.0; 3],
        };
        let mut out = vec![junk(0), junk(0), junk(5)];
        let mut ch = Channelizer::new(fs, 14e6).unwrap();
        ch.process_into(&iq, &mut out);
        let expected = Channelizer::new(fs, 14e6).unwrap().process(&iq);
        assert_eq!(expected.len(), 8);
        assert_eq!(out.len(), expected.len());
        assert_hops_bitwise_equal(&expected, &out);
        for h in &out {
            assert_eq!((h.x.len(), h.power.len()), (n, n));
        }
    }

    #[test]
    fn process_into_keeps_at_most_spare_retain_hops_surplus() {
        let fs = 192_000.0;
        let mut ch = Channelizer::new(fs, 14e6).unwrap();
        let (ln, hop) = (ch.filter_len(), ch.hop());
        let iq = noise_and_tones(fs, ln + 99 * hop + 4096);
        let mut out = Vec::new();
        ch.process_into(&iq[..ln + 99 * hop], &mut out);
        assert_eq!(out.len(), 100);
        ch.process_into(&iq[ln + 99 * hop..], &mut out);
        assert_eq!(out.len(), 8);
        assert!(
            ch.spare.len() <= SPARE_RETAIN_HOPS,
            "{} spare hops kept",
            ch.spare.len()
        );
    }

    const FS: f64 = 96_000.0;

    fn tone(freq: f64, n: usize, amp: f32, fs: f64) -> Vec<Complex32> {
        (0..n)
            .map(|i| {
                let phi = 2.0 * std::f64::consts::PI * freq * i as f64 / fs;
                Complex32::new(amp * phi.cos() as f32, amp * phi.sin() as f32)
            })
            .collect()
    }

    /// Channel index for offset_hz at the given N/fs (SPEC §1.1's f(k)
    /// inverted): `k = ((round(offset/delta)) mod N + N) mod N`.
    fn channel_for_offset(offset_hz: f64, n: usize, fs: f64) -> usize {
        let delta = fs / n as f64;
        let k_signed = (offset_hz / delta).round() as i64;
        k_signed.rem_euclid(n as i64) as usize
    }

    #[test]
    fn rejects_non_table_rate() {
        assert!(Channelizer::new(44_100.0, 0.0).is_err());
        assert!(Channelizer::new(96_000.0, 0.0).is_ok());
        assert!(Channelizer::new(192_000.0, 0.0).is_ok());
    }

    #[test]
    fn dimensions_match_spec_table() {
        let ch = Channelizer::new(96_000.0, 0.0).unwrap();
        assert_eq!(ch.n_channels(), 1024);
        assert_eq!(ch.hop(), 256);
        let ch = Channelizer::new(192_000.0, 0.0).unwrap();
        assert_eq!(ch.n_channels(), 2048);
        assert_eq!(ch.hop(), 512);
    }

    #[test]
    fn channel_freq_hz_matches_spec_f_of_k() {
        let ch = Channelizer::new(96_000.0, 14_000_000.0).unwrap();
        // k=0 is DC (center frequency itself).
        assert!((ch.channel_freq_hz(0) - 14_000_000.0).abs() < 1e-6);
        // k=1 is one channel above center.
        assert!((ch.channel_freq_hz(1) - (14_000_000.0 + 93.75)).abs() < 1e-6);
        // k = N-1 wraps to one channel BELOW center (negative-frequency side).
        assert!((ch.channel_freq_hz(1023) - (14_000_000.0 - 93.75)).abs() < 1e-6);
    }

    #[test]
    fn on_channel_tone_settles_near_unity_in_its_own_channel() {
        // Tone exactly at channel k0=64's center: offset = 64*93.75 = 6000 Hz.
        let mut ch = Channelizer::new(FS, 0.0).unwrap();
        let k0 = channel_for_offset(6_000.0, ch.n_channels(), FS);
        assert_eq!(k0, 64);
        let iq = tone(6_000.0, 192_000, 1.0, FS);
        let hops = ch.process(&iq);
        let warmup = ch.filter_len() / ch.hop() + 1;
        for hop in &hops[warmup..] {
            let mag = hop.power[k0].sqrt();
            assert!((mag - 1.0).abs() < 0.05, "channel {k0} magnitude {mag}");
        }
    }

    #[test]
    fn tone_150hz_away_is_rejected_by_80db_in_the_home_channel() {
        // SPEC §1.2: alias rejection >= 80 dB from ~108 Hz (1.15 channels) away.
        // A tone at k0's center + 150 Hz lands in a DIFFERENT home channel;
        // k0 itself should show near-zero power from it.
        let mut ch = Channelizer::new(FS, 0.0).unwrap();
        let k0 = channel_for_offset(6_000.0, ch.n_channels(), FS);
        let iq = tone(6_000.0 + 150.0, 192_000, 1.0, FS);
        let hops = ch.process(&iq);
        let warmup = ch.filter_len() / ch.hop() + 1;
        for hop in &hops[warmup..] {
            let mag = hop.power[k0].sqrt();
            assert!(mag < 2e-4, "stopband leak into channel {k0}: {mag}"); // -74 dB, slack for f32
        }
    }

    #[test]
    fn channel_edge_is_minus_6_db_in_both_neighbors() {
        // Tone exactly between channels k0 and k0+1 (edge = k0*Delta + Delta/2).
        let mut ch = Channelizer::new(FS, 0.0).unwrap();
        let k0 = channel_for_offset(6_000.0, ch.n_channels(), FS);
        let edge_hz = 6_000.0 + 93.75 / 2.0;
        let iq = tone(edge_hz, 384_000, 1.0, FS);
        let hops = ch.process(&iq);
        let warmup = ch.filter_len() / ch.hop() + 1;
        let steady = &hops[warmup..];
        let mean_mag = |k: usize| -> f32 {
            let sum: f32 = steady.iter().map(|h| h.power[k].sqrt()).sum();
            sum / steady.len() as f32
        };
        // -6 dB = 0.501 in amplitude; both neighbors should sit near 0.5.
        assert!(
            (mean_mag(k0) - 0.5).abs() < 0.05,
            "k0 edge gain {}",
            mean_mag(k0)
        );
        assert!(
            (mean_mag(k0 + 1) - 0.5).abs() < 0.05,
            "k0+1 edge gain {}",
            mean_mag(k0 + 1)
        );
    }

    #[test]
    fn is_deterministic() {
        let iq = tone(6_000.0, 96_000, 1.0, FS);
        let mut ch_a = Channelizer::new(FS, 0.0).unwrap();
        let mut ch_b = Channelizer::new(FS, 0.0).unwrap();
        let hops_a = ch_a.process(&iq);
        let hops_b = ch_b.process(&iq);
        assert_eq!(hops_a.len(), hops_b.len());
        for (a, b) in hops_a.iter().zip(hops_b.iter()) {
            for (pa, pb) in a.power.iter().zip(b.power.iter()) {
                assert_eq!(pa.to_bits(), pb.to_bits());
            }
        }
    }

    #[test]
    fn power_db_matches_spec_epsilon() {
        // 10*log10(0 + 1e-20) = -200.0 exactly.
        assert!((power_db(0.0) - (-200.0)).abs() < 1e-9);
        // 10*log10(1.0 + 1e-20) ~ 0.0.
        assert!(power_db(1.0).abs() < 1e-6);
    }

    #[test]
    fn process_across_multiple_calls_matches_one_call() {
        let iq = tone(6_000.0, 20_000, 1.0, FS);
        let mut whole = Channelizer::new(FS, 0.0).unwrap();
        let hops_whole = whole.process(&iq);

        let mut chunked = Channelizer::new(FS, 0.0).unwrap();
        let mut hops_chunked = Vec::new();
        for chunk in iq.chunks(137) {
            hops_chunked.extend(chunked.process(chunk));
        }
        assert_eq!(hops_whole.len(), hops_chunked.len());
        for (a, b) in hops_whole.iter().zip(hops_chunked.iter()) {
            assert_eq!(a.x, b.x);
        }
    }

    /// MAN-203 fixture: a tone at +6 kHz plus a DC offset.
    fn tone_with_dc(n: usize) -> Vec<Complex32> {
        tone(6_000.0, n, 0.1, FS)
            .into_iter()
            .map(|s| s + Complex32::new(0.01, 0.007))
            .collect()
    }

    fn dc_cfg(mode: crate::condition::StageMode) -> ConditioningConfig {
        ConditioningConfig {
            dc: mode,
            ..ConditioningConfig::default()
        }
    }

    #[test]
    fn all_off_conditioning_is_bit_identical_to_new() {
        let iq = tone_with_dc(20_000);
        let mut plain = Channelizer::new(FS, 0.0).unwrap();
        let mut off =
            Channelizer::with_conditioning(FS, 0.0, &ConditioningConfig::default()).unwrap();
        assert!(off.conditioning().is_none());
        assert_hops_bitwise_equal(&plain.process(&iq), &off.process(&iq));
    }

    #[test]
    fn observe_never_changes_a_hop() {
        use crate::condition::StageMode;
        let iq = tone_with_dc(FS as usize);
        let mut plain = Channelizer::new(FS, 0.0).unwrap();
        let mut obs = Channelizer::with_conditioning(FS, 0.0, &dc_cfg(StageMode::Observe)).unwrap();
        assert_hops_bitwise_equal(&plain.process(&iq), &obs.process(&iq));
        let log = obs.conditioning().unwrap();
        let level = log.dc_level().unwrap();
        assert!(
            (level.re - 0.01).abs() < 1e-3 && (level.im - 0.007).abs() < 1e-3,
            "{level}"
        );
        assert_eq!(log.spans().count(), 0);
    }

    #[test]
    fn dc_apply_removes_the_centre_channel_offset() {
        use crate::condition::{Mark, Stage, StageMode};
        let iq = tone_with_dc(FS as usize);
        let mut plain = Channelizer::new(FS, 0.0).unwrap();
        let mut dc = Channelizer::with_conditioning(FS, 0.0, &dc_cfg(StageMode::Apply)).unwrap();
        let hp = plain.process(&iq);
        let hd = dc.process(&iq);
        assert_eq!(hp.len(), hd.len());
        // Average the last half second's powers, after the blocker settled.
        let tail = hp.len() / 2;
        let avg = |hops: &[HopOutput], k: usize| -> f64 {
            hops[tail..].iter().map(|h| h.power[k] as f64).sum::<f64>() / (hops.len() - tail) as f64
        };
        let k_tone = 64; // +6 kHz at 93.75 Hz spacing.
        let drop_dc = 10.0 * (avg(&hp, 0) / avg(&hd, 0)).log10();
        let tone_change = 10.0 * (avg(&hd, k_tone) / avg(&hp, k_tone)).log10();
        assert!(drop_dc >= 30.0, "channel 0 fell only {drop_dc} dB");
        assert!(tone_change.abs() <= 0.05, "the tone moved {tone_change} dB");
        let log = dc.conditioning().unwrap();
        assert_eq!(log.count(Stage::Dc, Mark::Corrected), iq.len() as u64);
    }

    #[test]
    fn conditioned_process_is_chunk_invariant() {
        use crate::condition::StageMode;
        let iq = tone_with_dc(30_000);
        let cfg = dc_cfg(StageMode::Apply);
        let mut whole = Channelizer::with_conditioning(FS, 0.0, &cfg).unwrap();
        let hops_whole = whole.process(&iq);
        for size in [1usize, 7, 255, 4096] {
            let mut chunked = Channelizer::with_conditioning(FS, 0.0, &cfg).unwrap();
            let mut hops = Vec::new();
            for chunk in iq.chunks(size) {
                hops.extend(chunked.process(chunk));
            }
            assert_hops_bitwise_equal(&hops_whole, &hops);
            assert_eq!(
                whole.conditioning().unwrap().count(
                    crate::condition::Stage::Dc,
                    crate::condition::Mark::Corrected
                ),
                chunked.conditioning().unwrap().count(
                    crate::condition::Stage::Dc,
                    crate::condition::Mark::Corrected
                )
            );
            assert_eq!(
                whole.conditioning().unwrap().spans().collect::<Vec<_>>(),
                chunked.conditioning().unwrap().spans().collect::<Vec<_>>()
            );
        }
    }

    /// MAN-203 fixture: [`tone_with_dc`] plus isolated impulses.
    fn tone_with_impulses(n: usize) -> Vec<Complex32> {
        let mut iq = tone_with_dc(n);
        for (i, v) in [(4_000, 1e8f32), (9_001, -4e3), (17_777, 1e9)] {
            iq[i] = Complex32::new(v, 0.5 * v);
        }
        iq
    }

    #[test]
    fn impulse_apply_is_chunk_invariant() {
        use crate::condition::{Mark, Stage, StageMode};
        let iq = tone_with_impulses(30_000);
        let cfg = ConditioningConfig {
            impulse: StageMode::Apply,
            dc: StageMode::Apply,
            ..Default::default()
        };
        let mut whole = Channelizer::with_conditioning(FS, 0.0, &cfg).unwrap();
        let hops_whole = whole.process(&iq);
        assert_eq!(
            whole
                .conditioning()
                .unwrap()
                .count(Stage::Impulse, Mark::Repaired),
            3
        );
        for size in [1usize, 13, 4096] {
            let mut chunked = Channelizer::with_conditioning(FS, 0.0, &cfg).unwrap();
            let mut hops = Vec::new();
            for chunk in iq.chunks(size) {
                hops.extend(chunked.process(chunk));
            }
            assert_hops_bitwise_equal(&hops_whole, &hops);
            assert_eq!(whole.conditioning(), chunked.conditioning());
        }
    }

    #[test]
    fn impulse_apply_loses_at_most_the_final_hop() {
        use crate::condition::StageMode;
        let iq = tone_with_dc(30_000);
        let cfg = ConditioningConfig {
            impulse: StageMode::Apply,
            ..ConditioningConfig::default()
        };
        let mut plain = Channelizer::new(FS, 0.0).unwrap();
        let mut imp = Channelizer::with_conditioning(FS, 0.0, &cfg).unwrap();
        let hp = plain.process(&iq);
        let hi = imp.process(&iq);
        assert!(hp.len() - hi.len() <= 1, "{} vs {}", hp.len(), hi.len());
        // No impulse: the hops it formed are the plain ones, bit for bit.
        assert_hops_bitwise_equal(&hp[..hi.len()], &hi);
    }

    #[test]
    fn impulse_observe_never_changes_a_hop() {
        use crate::condition::{Mark, Stage, StageMode};
        let iq = tone_with_impulses(30_000);
        let cfg = ConditioningConfig {
            impulse: StageMode::Observe,
            ..ConditioningConfig::default()
        };
        let mut plain = Channelizer::new(FS, 0.0).unwrap();
        let mut obs = Channelizer::with_conditioning(FS, 0.0, &cfg).unwrap();
        assert_hops_bitwise_equal(&plain.process(&iq), &obs.process(&iq));
        let log = obs.conditioning().unwrap();
        assert_eq!(log.count(Stage::Impulse, Mark::Flagged), 3);
        assert_eq!(log.count(Stage::Impulse, Mark::Repaired), 0);
    }

    #[test]
    fn iq_observe_and_withheld_apply_preserve_bypass() {
        use crate::condition::{iq::IqCalibration, IqStatus, Mark, Stage, StageMode};
        let mut input = tone(6_000.0, (FS * 5.2) as usize, 0.1, FS);
        for s in &mut input {
            s.im *= 1.1;
        }
        let baseline = Channelizer::new(FS, 14e6).unwrap().process(&input);
        for (mode, calibration) in [
            (StageMode::Observe, None),
            (StageMode::Apply, None),
            (
                StageMode::Apply,
                Some(IqCalibration {
                    gain_ratio: 1.1,
                    phase_deg: 0.0,
                    center_freq_hz: 14.03e6,
                }),
            ),
        ] {
            let cfg = ConditioningConfig {
                iq: mode,
                iq_calibration: calibration,
                ..Default::default()
            };
            let mut ch = Channelizer::with_conditioning(FS, 14e6, &cfg).unwrap();
            assert_hops_bitwise_equal(&baseline, &ch.process(&input));
            let log = ch.conditioning().unwrap();
            assert_eq!(
                log.count(Stage::Iq, Mark::Uncertain),
                if mode == StageMode::Apply {
                    input.len() as u64
                } else {
                    (FS * 5.0) as u64
                }
            );
            if mode == StageMode::Observe {
                let mut split = Channelizer::with_conditioning(FS, 14e6, &cfg).unwrap();
                let mut actual = Vec::new();
                for chunk in input.chunks(255) {
                    actual.extend(split.process(chunk));
                }
                assert_hops_bitwise_equal(&baseline, &actual);
                assert_eq!(ch.conditioning(), split.conditioning());
            }
            assert_eq!(log.count(Stage::Iq, Mark::Corrected), 0);
            if mode == StageMode::Apply {
                assert_eq!(log.iq_status(), IqStatus::CalibrationMismatch);
            }
        }
    }

    #[test]
    fn iq_and_combined_chains_are_chunk_invariant() {
        use crate::condition::{iq::IqCalibration, Mark, Stage, StageMode};
        let input = tone_with_impulses((FS * 5.2) as usize);
        for combined in [false, true] {
            let cfg = ConditioningConfig {
                iq: StageMode::Apply,
                iq_calibration: Some(IqCalibration {
                    gain_ratio: 1.05,
                    phase_deg: 2.0,
                    center_freq_hz: 14e6,
                }),
                iq_tone_hz: Some(6000.0),
                dc: if combined {
                    StageMode::Apply
                } else {
                    StageMode::Off
                },
                impulse: if combined {
                    StageMode::Apply
                } else {
                    StageMode::Off
                },
            };
            let mut whole = Channelizer::with_conditioning(FS, 14e6, &cfg).unwrap();
            let expected = whole.process(&input);
            assert_eq!(
                whole
                    .conditioning()
                    .unwrap()
                    .count(Stage::Iq, Mark::Corrected),
                input.len() as u64 - if combined { 21 } else { 0 }
            );
            for size in [1, 7, 255, 4096] {
                let mut split = Channelizer::with_conditioning(FS, 14e6, &cfg).unwrap();
                let mut actual = Vec::new();
                for chunk in input.chunks(size) {
                    actual.extend(split.process(chunk));
                }
                assert_hops_bitwise_equal(&expected, &actual);
                assert_eq!(whole.conditioning(), split.conditioning());
            }
        }
    }

    fn assert_hops_bitwise_equal(a: &[HopOutput], b: &[HopOutput]) {
        assert_eq!(a.len(), b.len());
        for (ha, hb) in a.iter().zip(b.iter()) {
            assert_eq!(ha.m, hb.m);
            for (xa, xb) in ha.x.iter().zip(hb.x.iter()) {
                assert_eq!(xa.re.to_bits(), xb.re.to_bits());
                assert_eq!(xa.im.to_bits(), xb.im.to_bits());
            }
            for (pa, pb) in ha.power.iter().zip(hb.power.iter()) {
                assert_eq!(pa.to_bits(), pb.to_bits());
            }
        }
    }

    #[test]
    fn a_bad_sample_is_replaced_before_it_reaches_any_hop() {
        // MAN-234: one NaN mid-stream must not reach channel power.
        let mut iq = tone(6_000.0, 20_000, 1.0, FS);
        iq[9_000].re = f32::NAN;
        let mut ch = Channelizer::new(FS, 0.0).unwrap();
        let hops = ch.process(&iq);
        assert!(!hops.is_empty());
        for hop in &hops {
            assert!(hop.power.iter().all(|p| p.is_finite()), "hop {}", hop.m);
        }
        assert_eq!(ch.bad_samples(), 1);
    }

    #[test]
    fn a_bad_sample_produces_the_same_hops_as_silence() {
        // MAN-234 D3: the replacement is exactly `0+0j`, so a bad sample's
        // hops equal those of the same input with that sample zeroed.
        let clean = tone(6_000.0, 20_000, 1.0, FS);
        let mut zeroed = clean.clone();
        zeroed[9_000] = Complex32::new(0.0, 0.0);
        for bad in [f32::INFINITY, f32::MAX] {
            let mut corrupt = clean.clone();
            corrupt[9_000].re = bad;
            let mut ch_bad = Channelizer::new(FS, 0.0).unwrap();
            let mut ch_zero = Channelizer::new(FS, 0.0).unwrap();
            let hops_bad = ch_bad.process(&corrupt);
            let hops_zero = ch_zero.process(&zeroed);
            assert_hops_bitwise_equal(&hops_bad, &hops_zero);
            assert_eq!(ch_bad.bad_samples(), 1);
            assert_eq!(ch_zero.bad_samples(), 0);
        }
    }

    #[test]
    fn bad_samples_accumulate_across_process_calls() {
        let mut a = tone(6_000.0, 5_000, 1.0, FS);
        let mut b = tone(6_000.0, 5_000, 1.0, FS);
        a[100].im = f32::NAN;
        b[4_000].re = f32::NEG_INFINITY;
        let mut ch = Channelizer::new(FS, 0.0).unwrap();
        ch.process(&a);
        assert_eq!(ch.bad_samples(), 1);
        ch.process(&b);
        assert_eq!(ch.bad_samples(), 2);

        // One bad sample whose chunk is split across a call boundary is
        // counted once, and the samples already in the window are not
        // re-checked or re-counted.
        let mut whole = tone(6_000.0, 20_000, 1.0, FS);
        whole[10_000].re = f32::NAN;
        let mut split = Channelizer::new(FS, 0.0).unwrap();
        split.process(&whole[..9_999]);
        split.process(&whole[9_999..10_002]);
        split.process(&whole[10_002..]);
        assert_eq!(split.bad_samples(), 1);
    }

    #[test]
    fn full_scale_bound_keeps_every_power_finite() {
        // MAN-234 D2: at the bound, DC (coherent worst case) and
        // alternating-sign input keep every channel power finite.
        let b = sample_guard::MAX_SAMPLE_MAGNITUDE;
        let mut ch = Channelizer::new(FS, 0.0).unwrap();
        let len = ch.filter_len() * 2;
        let dc = vec![Complex32::new(b, b); len];
        let alt: Vec<Complex32> = (0..len)
            .map(|i| {
                let v = if i % 2 == 0 { b } else { -b };
                Complex32::new(v, -v)
            })
            .collect();
        let mut hops = ch.process(&dc);
        hops.extend(ch.process(&alt));
        assert!(!hops.is_empty());
        for hop in &hops {
            assert!(hop.power.iter().all(|p| p.is_finite()), "hop {}", hop.m);
        }
        assert_eq!(ch.bad_samples(), 0);
    }

    /// MAN-114: a clean tone at `freq_hz`, channelized at `fs`; returns the
    /// `phase_position` (unwrapped near `k0`) of the summed
    /// `phase_lag_product` of each post-warm-up hop's strongest channel
    /// among `k0 - 1..=k0 + 1`.
    fn phase_position_of_tone(fs: f64, freq_hz: f64, k0: usize) -> f64 {
        let mut ch = Channelizer::new(fs, 0.0).unwrap();
        let n = ch.n_channels();
        let iq = tone(freq_hz, 2 * ch.filter_len() + 64 * ch.hop(), 1.0, fs);
        let hops = ch.process(&iq);
        let warmup = ch.filter_len() / ch.hop() + 1;
        let mut z = Complex64::new(0.0, 0.0);
        for pair in hops[warmup..].windows(2) {
            let k = [(k0 + n - 1) % n, k0, (k0 + 1) % n]
                .into_iter()
                .max_by(|&a, &b| pair[1].power[a].total_cmp(&pair[1].power[b]))
                .unwrap();
            z += phase_lag_product(&pair[0], &pair[1], k).expect("consecutive full hops");
        }
        phase_position(z, k0 as f64).expect("a tone gives a non-zero sum")
    }

    #[test]
    fn phase_position_is_exact_for_a_clean_tone_across_the_channel() {
        // MAN-114: the dB parabola's S-curve bias (up to ~12 Hz on a clean
        // tone) is absent from the phase estimator on every table, on even
        // and odd channels, at every fractional offset.
        for fs in [48_000.0, 96_000.0, 192_000.0] {
            for k0 in [64usize, 65] {
                for step in 0..=20 {
                    let frac = -0.5 + 0.05 * step as f64;
                    let truth = k0 as f64 + frac;
                    let got = phase_position_of_tone(fs, truth * CHANNEL_SPACING_HZ, k0);
                    let err_hz = (got - truth) * CHANNEL_SPACING_HZ;
                    assert!(
                        err_hz.abs() < 0.05,
                        "fs {fs} k0 {k0} frac {frac:.2}: phase position {got} vs {truth} \
                         ({err_hz:+.4} Hz)"
                    );
                }
            }
        }
    }

    #[test]
    fn phase_position_handles_negative_frequency_channels() {
        // -6000 Hz is channel N-64 (SPEC §1.1's f(k)); 0.3 channel below it.
        let n = (FS / CHANNEL_SPACING_HZ) as usize;
        let k0 = n - 64;
        let truth = k0 as f64 - 0.3;
        let got = phase_position_of_tone(FS, -6_000.0 - 0.3 * CHANNEL_SPACING_HZ, k0);
        let err_hz = (got - truth) * CHANNEL_SPACING_HZ;
        assert!(
            err_hz.abs() < 0.05,
            "phase position {got} vs {truth} ({err_hz:+.4} Hz)"
        );
    }

    #[test]
    fn phase_lag_product_none_on_hop_gap_or_power_only_hop() {
        let full = |m: u64| HopOutput {
            m,
            x: vec![Complex32::new(1.0, 0.5); 8],
            power: vec![1.25; 8],
        };
        assert!(phase_lag_product(&full(4), &full(5), 3).is_some());
        // Not consecutive: a hop gap (or a reset stream) breaks the lag.
        assert_eq!(phase_lag_product(&full(4), &full(6), 3), None);
        assert_eq!(phase_lag_product(&full(5), &full(5), 3), None);
        // Power-only hops (an empty `x`) carry no phase.
        let power_only = |m: u64| HopOutput {
            m,
            x: Vec::new(),
            power: vec![1.25; 8],
        };
        assert_eq!(phase_lag_product(&full(4), &power_only(5), 3), None);
        assert_eq!(phase_lag_product(&power_only(4), &full(5), 3), None);
        // Out-of-range channel.
        assert_eq!(phase_lag_product(&full(4), &full(5), 8), None);
    }

    #[test]
    fn phase_position_unwraps_near_reference() {
        // arg = 2π·0.25 is position 1.0 (mod 4); nearest 9.1 is 9.0.
        let z = Complex64::from_polar(2.5, std::f64::consts::TAU * 0.25);
        let p = phase_position(z, 9.1).unwrap();
        assert!((p - 9.0).abs() < 1e-12, "got {p}");
        // Near a negative-side reference too: nearest 1022.6 is 1021.0
        // (1021 ≡ 1 mod 4) rather than 1025.0.
        let p = phase_position(z, 1022.6).unwrap();
        assert!((p - 1021.0).abs() < 1e-9, "got {p}");
        assert_eq!(phase_position(Complex64::new(0.0, 0.0), 9.1), None);
    }

    #[test]
    fn interpolate_offset_finds_symmetric_peak_at_zero() {
        // A true local max with equal neighbors -> delta = 0.
        assert_eq!(interpolate_offset(0.5, 1.0, 0.5), Some(0.0));
    }

    #[test]
    fn interpolate_offset_leans_toward_the_larger_neighbor() {
        // Peak biased toward p_plus -> positive delta (SPEC §1.4 formula
        // sign convention: delta = 0.5*(P_minus - P_plus)/denom).
        let d = interpolate_offset(0.3, 1.0, 0.6).unwrap();
        assert!(d > 0.0, "delta {d}");
        assert!(d <= 0.5);
    }

    #[test]
    fn interpolate_offset_clamps_to_half_bin() {
        // Extremely asymmetric neighbors would produce |delta| > 0.5 unclamped.
        let d = interpolate_offset(1e-6, 1.0, 0.999).unwrap();
        assert!((-0.5..=0.5).contains(&d));
    }

    #[test]
    fn interpolate_offset_none_when_not_a_local_max() {
        // A local MINIMUM at the center bin (a valley, not a peak): the
        // denominator is computed on dB-converted values, and log10 is
        // concave, so even monotonically-increasing linear power values
        // produce a local-max-shaped (negative) denominator in dB -- a
        // genuine valley is the unambiguous "no local max" case.
        // denom = db(0.5) - 2*db(0.1) + db(0.5) ~= 13.98 >= 0 -> unusable.
        assert_eq!(interpolate_offset(0.5, 0.1, 0.5), None);
    }

    #[test]
    fn odd_channel_sign_correction_recovers_a_phase_continuous_odd_channel_tone() {
        // Codex review, PR #161's MAN-168: a centered tone on an odd
        // channel comes out of the channelizer with a spurious per-hop
        // sign flip (`(-1)^(m*k)`) that a magnitude-only consumer never
        // noticed; a consumer using the complex spectrum directly (the
        // narrowband refiner) sees severe self-cancellation without this
        // correction. Verify the corrected odd channel matches an even
        // channel's amplitude exactly, using the real channelizer.
        use crate::refine::Refiner;
        let fs = 96_000.0;
        let mean = |k: usize, corrected: bool| -> f32 {
            let mut ch = Channelizer::new(fs, 0.0).unwrap();
            let freq_hz = ch.channel_freq_hz(k);
            let sig = tone(freq_hz, 60_000, 1.0, fs);
            let hops = ch.process(&sig);
            let mut r = Refiner::new(30.0, 0.0);
            let mut sum = 0.0f32;
            let mut count = 0u32;
            for h in hops.iter().skip(20) {
                let x = if corrected {
                    h.x[k] * odd_channel_sign_correction(h.m, k)
                } else {
                    h.x[k]
                };
                sum += r.push(x);
                count += 1;
            }
            sum / count as f32
        };
        let even_amp = mean(64, true);
        let odd_uncorrected = mean(65, false);
        let odd_corrected = mean(65, true);
        assert!(
            odd_uncorrected < even_amp * 0.5,
            "sanity: an uncorrected odd channel must show real self-cancellation, got \
             odd={odd_uncorrected} even={even_amp}"
        );
        assert!(
            (odd_corrected - even_amp).abs() < 1e-3,
            "corrected odd channel must match the even channel's amplitude exactly, got \
             odd_corrected={odd_corrected} even={even_amp}"
        );
    }
}
