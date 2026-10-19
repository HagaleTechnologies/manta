//! Real-to-analytic (Hilbert) conversion: odd-length windowed-sinc FIR,
//! Kaiser-windowed identically to the PFB prototype (proto.rs). Used both
//! for live audio input (manta-input::AudioIqSource) and offline
//! Watterson vector rendering (manta-testkit). Design doc §3.

use crate::proto::{bessel_i0, KAISER_BETA};
use num_complex::Complex32;

/// Hilbert FIR length (odd) for live decode. MAN-273: the M1 design's
/// `HILBERT_TAPS_M1_LEGACY` taps left a real tone's negative-frequency
/// image only ~43 dB down at 750 Hz, and the per-channel detector spawned
/// and churned tracks on that leak. 511 taps holds at least
/// `HILBERT_MIN_IMAGE_REJECTION_DB` across
/// `[hilbert_guard_hz(fs), fs/2 - hilbert_guard_hz(fs)]`, asserted by
/// `image_rejection_meets_the_guaranteed_band_contract` below. See
/// `docs/DECISIONS/2026-10-16-man273-hilbert-image-guard.md`.
pub const HILBERT_TAPS: usize = 511;

/// The M1 design's length, kept only for test-fixture synthesis
/// (`manta-testkit`'s Watterson and RTTY renders), so widening
/// `HILBERT_TAPS` cannot move a golden-vector byte. Never used for live
/// decode (MAN-273 D4).
pub const HILBERT_TAPS_M1_LEGACY: usize = 129;

/// Enforced image-rejection floor across the guaranteed band, in dB. The
/// Kaiser design reaches ~80 dB and more; 70 dB leaves headroom for f32 tap
/// quantization and the finite DFT the contract test measures with.
pub const HILBERT_MIN_IMAGE_REJECTION_DB: f64 = 70.0;

/// The guard at 48 kHz, in Hz: 600 Hz either side of DC and of Nyquist.
const HILBERT_GUARD_HZ_AT_48K: f64 = 600.0;

/// Offset from DC (and, symmetrically, from Nyquist) inside which a
/// `HILBERT_TAPS` transformer at `fs` does not guarantee
/// `HILBERT_MIN_IMAGE_REJECTION_DB` of image rejection, in Hz. The FIR's
/// response scales with the sample rate, so the guard does too: 600 Hz at
/// 48 kHz. Sources built on this transformer declare it through
/// `IqSource::analytic_guard_hz` so the detector spawns no track there
/// (MAN-273).
pub fn hilbert_guard_hz(fs: f64) -> f64 {
    HILBERT_GUARD_HZ_AT_48K * fs / 48_000.0
}

/// Design a length-`taps` windowed-sinc Hilbert FIR:
/// h\[n\] = 0 for (n - center) even, -2 / (pi * (n - center)) for odd,
/// Kaiser-windowed with the PFB prototype's beta (proto.rs). The negative
/// sign is required because `process()` pairs `taps[i]` directly with
/// `hist[i]` (oldest-first), which reverses the convolution index order
/// relative to standard causal FIR. Since the ideal Hilbert kernel is
/// antisymmetric, this index reversal is algebraically equivalent to
/// negating the kernel.
///
/// Panics unless `taps` is odd and >= 3. An even length has no integer
/// center tap; `taps == 0` underflows `len - 1`; and `taps == 1` makes the
/// window's `t = 2*i/(len-1) - 1` divide zero by zero, so `bessel_i0(NaN)`
/// never meets its convergence test and the design hangs instead of
/// failing.
pub fn design_hilbert_fir_n(taps: usize) -> Vec<f32> {
    assert!(
        taps % 2 == 1 && taps >= 3,
        "Hilbert FIR length must be odd and >= 3, got {taps}"
    );
    let len = taps;
    let center = (len - 1) as f64 / 2.0; // integer-valued since len is odd
    let i0_beta = bessel_i0(KAISER_BETA);
    let mut h = vec![0.0f64; len];
    for (i, tap) in h.iter_mut().enumerate() {
        let k = i as f64 - center;
        let ideal = if (k as i64) % 2 == 0 {
            0.0
        } else {
            -2.0 / (std::f64::consts::PI * k)
        };
        let t = 2.0 * i as f64 / (len - 1) as f64 - 1.0;
        let w = bessel_i0(KAISER_BETA * (1.0 - t * t).sqrt()) / i0_beta;
        *tap = ideal * w;
    }
    h.into_iter().map(|v| v as f32).collect()
}

/// Design the default, `HILBERT_TAPS`-long Hilbert FIR.
pub fn design_hilbert_fir() -> Vec<f32> {
    design_hilbert_fir_n(HILBERT_TAPS)
}

/// Streaming FIR Hilbert transformer: incrementally converts real samples
/// to an analytic (I = delayed real, Q = Hilbert-filtered) signal. Causal,
/// fixed group delay of `(taps - 1) / 2` samples, callable across multiple
/// `process` calls with persistent history (design doc §3).
pub struct HilbertTransformer {
    taps: Vec<f32>,
    /// Indices of the nonzero taps. The kernel is exactly zero at every
    /// even offset from center, so `process()` skips those products; the
    /// running f64 sum is bit-identical to the dense one (asserted by
    /// `sparse_tap_evaluation_is_bit_identical_to_dense`). For 511 taps the
    /// center index 255 is odd, so the two outermost taps are odd-offset
    /// and nonzero: 256 of 511.
    nz: Vec<usize>,
    /// Ring of the last `taps.len()` real input samples, oldest first.
    hist: std::collections::VecDeque<f32>,
}

impl Default for HilbertTransformer {
    fn default() -> Self {
        Self::new()
    }
}

impl HilbertTransformer {
    /// A `HILBERT_TAPS`-long transformer, the live-decode default.
    pub fn new() -> Self {
        Self::with_taps(HILBERT_TAPS)
    }

    /// A transformer with an explicit tap count. Panics unless `taps` is
    /// odd and >= 3 (see `design_hilbert_fir_n`).
    pub fn with_taps(taps: usize) -> Self {
        let taps_vec = design_hilbert_fir_n(taps);
        let nz = taps_vec
            .iter()
            .enumerate()
            .filter(|(_, &t)| t != 0.0)
            .map(|(i, _)| i)
            .collect();
        HilbertTransformer {
            taps: taps_vec,
            nz,
            hist: std::collections::VecDeque::from(vec![0.0f32; taps]),
        }
    }

    /// Fixed causal group delay, in samples: `(taps - 1) / 2`.
    pub fn delay(&self) -> usize {
        (self.taps.len() - 1) / 2
    }

    /// Convert one chunk of real samples to analytic Complex32 samples, one
    /// output per input, using a persistent history window across calls.
    pub fn process(&mut self, input: &[f32]) -> Vec<Complex32> {
        let center = self.delay();
        let mut out = Vec::with_capacity(input.len());
        for &x in input {
            self.hist.pop_front();
            self.hist.push_back(x);
            // Sequential f64 accumulation (SPEC §6.4 determinism convention)
            // over the nonzero taps only.
            let mut acc = 0.0f64;
            for &i in &self.nz {
                acc += self.taps[i] as f64 * self.hist[i] as f64;
            }
            let re = self.hist[center];
            out.push(Complex32::new(re, acc as f32));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fir_is_odd_length_and_zero_at_even_offsets() {
        let h = design_hilbert_fir();
        assert_eq!(h.len(), HILBERT_TAPS);
        let center = (h.len() - 1) / 2;
        assert_eq!(h[center], 0.0, "center tap (k=0) must be exactly zero");
        assert_eq!(h[center + 2], 0.0, "k=+2 (even) must be exactly zero");
        assert_eq!(h[center - 2], 0.0, "k=-2 (even) must be exactly zero");
        assert!(h[center + 1] != 0.0, "k=+1 (odd) must be nonzero");
    }

    #[test]
    fn fir_is_antisymmetric() {
        // Ideal Hilbert kernel h[n] = 2/(pi*n) is odd: h[center+k] = -h[center-k].
        let h = design_hilbert_fir();
        let center = (h.len() - 1) / 2;
        for k in 1..center {
            assert!(
                (h[center + k] + h[center - k]).abs() < 1e-6,
                "k={k}: h+={} h-={}",
                h[center + k],
                h[center - k]
            );
        }
    }

    #[test]
    fn real_branch_is_delay_matched() {
        let mut h = HilbertTransformer::new();
        let x: Vec<f32> = (0..500).map(|i| (i as f32 * 0.01).sin()).collect();
        let y = h.process(&x);
        let delay = h.delay();
        for i in delay..x.len() {
            assert_eq!(y[i].re, x[i - delay], "real branch mismatch at {i}");
        }
    }

    #[test]
    fn analytic_signal_of_positive_tone_rotates_forward() {
        let fs = 48_000.0f64;
        let f = 1_000.0f64; // well within the Hilbert passband
        let n = 4096;
        let x: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * f * i as f64 / fs).cos() as f32)
            .collect();
        let mut h = HilbertTransformer::new();
        let y = h.process(&x);
        let delay = h.delay();
        let skip = 2 * delay; // let the filter's transient settle at both ends
                              // Explicit range form is clearer than enumerate/skip/take here since
                              // `i` is used both for the y[i] index and in the assert failure message.
        #[allow(clippy::needless_range_loop)]
        for i in skip..(n - skip) {
            assert!(
                (y[i].norm() - 1.0).abs() < 0.05,
                "i={i} norm={}",
                y[i].norm()
            );
        }
        let expect_dphi = 2.0 * std::f64::consts::PI * f / fs;
        let i = n / 2;
        let dphi = (y[i + 1].im.atan2(y[i + 1].re) - y[i].im.atan2(y[i].re)) as f64;
        let dphi = dphi.rem_euclid(std::f64::consts::TAU);
        assert!(
            (dphi - expect_dphi).abs() < 0.01,
            "dphi={dphi} expect={expect_dphi}"
        );
    }

    #[test]
    fn streams_across_multiple_process_calls() {
        // Splitting one input into chunks must produce byte-identical
        // output to processing it in one call -- the history state persists.
        let fs = 48_000.0f64;
        let f = 700.0f64;
        let n = 2000;
        let x: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * f * i as f64 / fs).cos() as f32)
            .collect();
        let whole = HilbertTransformer::new().process(&x);
        let mut chunked = HilbertTransformer::new();
        let mut out = Vec::new();
        for chunk in x.chunks(137) {
            out.extend(chunked.process(chunk));
        }
        assert_eq!(whole, out);
    }

    /// The 129-tap design every frozen Watterson/RTTY fixture was rendered
    /// with, computed exactly as the M1 `design_hilbert_fir()` computed it
    /// (that function had no length parameter; its body is inlined here).
    fn m1_design() -> Vec<f32> {
        let len = 129usize;
        let center = (len - 1) as f64 / 2.0;
        let i0_beta = bessel_i0(KAISER_BETA);
        (0..len)
            .map(|i| {
                let k = i as f64 - center;
                let ideal = if (k as i64) % 2 == 0 {
                    0.0
                } else {
                    -2.0 / (std::f64::consts::PI * k)
                };
                let t = 2.0 * i as f64 / (len - 1) as f64 - 1.0;
                let w = bessel_i0(KAISER_BETA * (1.0 - t * t).sqrt()) / i0_beta;
                (ideal * w) as f32
            })
            .collect()
    }

    #[test]
    fn legacy_taps_reproduce_the_m1_design_bit_exactly() {
        assert_eq!(HILBERT_TAPS_M1_LEGACY, 129);
        assert_eq!(design_hilbert_fir_n(HILBERT_TAPS_M1_LEGACY), m1_design());
    }

    #[test]
    #[should_panic(expected = "odd and >= 3")]
    fn with_taps_rejects_zero() {
        HilbertTransformer::with_taps(0);
    }

    #[test]
    #[should_panic(expected = "odd and >= 3")]
    fn with_taps_rejects_one_instead_of_hanging() {
        // taps == 1 is odd; without the >= 3 bound the design loop hangs on
        // bessel_i0(NaN) rather than panicking.
        HilbertTransformer::with_taps(1);
    }

    #[test]
    #[should_panic(expected = "odd and >= 3")]
    fn with_taps_rejects_two() {
        HilbertTransformer::with_taps(2);
    }

    #[test]
    #[should_panic(expected = "odd and >= 3")]
    fn with_taps_rejects_an_even_length() {
        HilbertTransformer::with_taps(128);
    }

    #[test]
    #[should_panic(expected = "odd and >= 3")]
    fn design_hilbert_fir_n_rejects_an_even_length_directly() {
        design_hilbert_fir_n(128);
    }

    #[test]
    fn sparse_tap_evaluation_is_bit_identical_to_dense() {
        let taps = design_hilbert_fir_n(511);
        assert_eq!(taps.iter().filter(|t| **t != 0.0).count(), 256);
        assert_eq!(taps.iter().filter(|t| **t == 0.0).count(), 255);
        let center = (taps.len() - 1) / 2;
        // Signed, non-trivial input, so (-0.0) + 0.0 cases occur.
        let x: Vec<f32> = (0..4096)
            .map(|i| ((i as f32 * 0.017).sin() - 0.5) * if i % 7 == 0 { -1.0 } else { 1.0 })
            .collect();
        let got = HilbertTransformer::with_taps(511).process(&x);
        let mut hist = std::collections::VecDeque::from(vec![0.0f32; taps.len()]);
        let want: Vec<Complex32> = x
            .iter()
            .map(|&s| {
                hist.pop_front();
                hist.push_back(s);
                let mut acc = 0.0f64;
                for (&h, &v) in taps.iter().zip(hist.iter()) {
                    acc += h as f64 * v as f64;
                }
                Complex32::new(hist[center], acc as f32)
            })
            .collect();
        assert_eq!(got, want, "sparse evaluation diverged from dense");
    }

    #[test]
    fn hilbert_guard_hz_is_rate_derived() {
        assert_eq!(hilbert_guard_hz(48_000.0), 600.0);
        assert_eq!(hilbert_guard_hz(96_000.0), 1200.0);
    }

    #[test]
    fn delay_is_instance_dependent() {
        assert_eq!(HilbertTransformer::with_taps(511).delay(), 255);
        assert_eq!(HilbertTransformer::with_taps(129).delay(), 64);
        assert_eq!(HilbertTransformer::new().delay(), (HILBERT_TAPS - 1) / 2);
    }

    /// Image rejection at `f_hz` through the real streaming `process()`, in
    /// dB: a pure cosine in, then the ratio of the wanted +f component to
    /// the leaked -f component over the settled middle, each measured with
    /// a Hann-windowed single-bin DFT.
    fn image_rejection_db(taps: usize, f_hz: f64, fs: f64) -> f64 {
        let n = 8 * taps;
        let skip = taps - 1; // both filter transients
        let x: Vec<f32> = (0..n + 2 * skip)
            .map(|i| (std::f64::consts::TAU * f_hz * i as f64 / fs).cos() as f32)
            .collect();
        let y = HilbertTransformer::with_taps(taps).process(&x);
        let seg = &y[skip..skip + n];
        let (mut pos, mut neg) = (
            num_complex::Complex64::new(0.0, 0.0),
            num_complex::Complex64::new(0.0, 0.0),
        );
        for (i, s) in seg.iter().enumerate() {
            let t = i as f64;
            let w = 0.5 - 0.5 * (std::f64::consts::TAU * t / n as f64).cos();
            let ph = std::f64::consts::TAU * f_hz * t / fs;
            let v = num_complex::Complex64::new(s.re as f64, s.im as f64) * w;
            pos += v * num_complex::Complex64::from_polar(1.0, -ph);
            neg += v * num_complex::Complex64::from_polar(1.0, ph);
        }
        20.0 * (pos.norm() / neg.norm()).log10()
    }

    #[test]
    fn image_rejection_meets_the_guaranteed_band_contract() {
        let fs = 48_000.0;
        let guard = hilbert_guard_hz(fs);
        // 50 Hz grid across the guaranteed band, plus witnesses (the tone
        // MAN-273 was found on, rig sidetones, the near-Nyquist rim).
        let mut freqs: Vec<f64> = Vec::new();
        let mut f = guard;
        while f <= fs / 2.0 - guard {
            freqs.push(f);
            f += 50.0;
        }
        freqs.extend([750.0, 1000.0, 2000.0, 3000.0, fs / 2.0 - guard]);
        for f in freqs {
            let r = image_rejection_db(HILBERT_TAPS, f, fs);
            assert!(
                r >= HILBERT_MIN_IMAGE_REJECTION_DB,
                "{f} Hz: {r:.1} dB image rejection, want >= {HILBERT_MIN_IMAGE_REJECTION_DB}"
            );
        }
    }

    #[test]
    fn the_m1_design_misses_the_contract_inside_the_band() {
        // Why the default moved: the legacy design leaks at the tone the
        // defect was found on (43.1 dB measured at 750 Hz).
        let r = image_rejection_db(HILBERT_TAPS_M1_LEGACY, 750.0, 48_000.0);
        assert!(r < 60.0, "legacy design measured {r:.1} dB at 750 Hz");
    }
}
