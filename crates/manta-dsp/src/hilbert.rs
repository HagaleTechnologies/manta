//! Real-to-analytic (Hilbert) conversion: odd-length windowed-sinc FIR,
//! Kaiser-windowed identically to the PFB prototype (proto.rs). Used both
//! for live audio input (manta-input::AudioIqSource) and offline
//! Watterson vector rendering (manta-testkit). Design doc §3.
//!
//! MAN-4: image rejection below `HILBERT_GUARD_HZ` from DC (and,
//! symmetrically, from +/-Nyquist) is not guaranteed at any tap count --
//! `image_rejection_meets_the_guaranteed_band_contract` (below) is the
//! authority on what this filter actually delivers, not this prose. See
//! docs/DECISIONS/2026-09-04-man-4-hilbert-guard-pins.md.

use crate::proto::{bessel_i0, KAISER_BETA};
use num_complex::Complex32;

/// Hilbert FIR length (odd). See MAN-4: 129 taps (M1) gave only ~43 dB of
/// image rejection at 750 Hz, and the leaked negative-frequency image
/// spawned spurious tracks once the real per-channel detector (SPEC §2,
/// `manta-engine::track::TrackManager`) landed. 511 taps reaches the Kaiser
/// beta=7.857 design floor (~80 dB) by `HILBERT_GUARD_HZ`; see
/// docs/DECISIONS/2026-09-04-man-4-hilbert-guard-pins.md.
pub const HILBERT_TAPS: usize = 511;

/// The M1 129-tap design, retained *only* for golden-vector fixture
/// rendering (`manta-testkit::scene`), so this fix cannot move the
/// V1-V10 byte baseline. Not for any live-decode analysis path. See the
/// MAN-4 pin doc, decision D4.
pub const HILBERT_TAPS_M1_LEGACY: usize = 129;

/// Below this offset from DC (and, symmetrically, from Nyquist) no finite
/// Hilbert FIR delivers usable image rejection. Sources built on this
/// transformer report it via `manta_input::IqSource::analytic_guard_hz` so
/// the detector declines to spawn tracks there (MAN-4).
pub const HILBERT_GUARD_HZ: f64 = 300.0;

/// Contract asserted by `image_rejection_meets_the_guaranteed_band_contract`.
/// The design target is the Kaiser stopband (~80 dB); 70 dB is the enforced
/// floor, leaving headroom for f32 tap quantization and DFT measurement.
pub const HILBERT_MIN_IMAGE_REJECTION_DB: f64 = 70.0;

/// Design a length-`taps` windowed-sinc Hilbert FIR (`taps` must be odd):
/// h\[n\] = 0 for (n - center) even, -2 / (pi * (n - center)) for odd,
/// Kaiser-windowed with the PFB prototype's beta (proto.rs). The negative
/// sign is required because `process()` pairs `taps[i]` directly with
/// `hist[i]` (oldest-first), which reverses the convolution index order
/// relative to standard causal FIR. Since the ideal Hilbert kernel is
/// antisymmetric, this index reversal is algebraically equivalent to
/// negating the kernel.
pub fn design_hilbert_fir_n(taps: usize) -> Vec<f32> {
    assert!(taps % 2 == 1, "Hilbert FIR length must be odd, got {taps}");
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

/// Design the default length-`HILBERT_TAPS` Hilbert FIR.
pub fn design_hilbert_fir() -> Vec<f32> {
    design_hilbert_fir_n(HILBERT_TAPS)
}

/// Streaming FIR Hilbert transformer: incrementally converts real samples
/// to an analytic (I = delayed real, Q = Hilbert-filtered) signal. Causal,
/// fixed group delay of (taps-1)/2 samples (see `delay()`), callable across
/// multiple `process` calls with persistent history (design doc §3).
pub struct HilbertTransformer {
    taps: Vec<f32>,
    /// Indices into `taps`/`hist` whose tap is nonzero. The ideal Hilbert
    /// kernel is exactly zero at every even offset from center, so this is
    /// roughly half of `taps.len()` (for an odd-length kernel, exactly
    /// `(taps.len()+1)/2`, since the always-zero center tap sits in the
    /// even-offset half); iterating only these roughly halves the
    /// per-sample cost and is bit-identical to the dense loop (asserted by
    /// `sparse_tap_evaluation_is_bit_identical_to_dense`; MAN-4 D3).
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
    pub fn new() -> Self {
        Self::with_taps(HILBERT_TAPS)
    }

    /// A transformer with an explicit (odd) tap count. Panics on an even
    /// length: the design relies on an integer center tap. MAN-4: lets
    /// `manta-testkit::scene` freeze fixture rendering at
    /// `HILBERT_TAPS_M1_LEGACY` while the live-decode default widens.
    pub fn with_taps(taps: usize) -> Self {
        let taps = design_hilbert_fir_n(taps);
        let nz = taps
            .iter()
            .enumerate()
            .filter(|(_, t)| **t != 0.0)
            .map(|(i, _)| i)
            .collect();
        let hist = std::collections::VecDeque::from(vec![0.0f32; taps.len()]);
        HilbertTransformer { taps, nz, hist }
    }

    /// Fixed causal group delay, in samples: (taps.len() - 1) / 2. Reads
    /// the instance's own tap count, not the `HILBERT_TAPS` default -- MAN-4
    /// made this instance-dependent via `with_taps`.
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
            // Sequential f64 accumulation over only the structurally-nonzero
            // taps (SPEC §6.4 determinism convention; MAN-4 D2/D3 -- ascending
            // index order preserved, so this is bit-identical to summing
            // every tap).
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

    /// Measure real->analytic image rejection at `f_hz`, in dB: the ratio of
    /// the wanted +f component to the leaked -f component of the analytic
    /// output. This is the property MAN-4 turns on and the only property of
    /// this filter that the live-audio path actually depends on.
    fn image_rejection_db(taps: usize, f_hz: f64, fs: f64) -> f64 {
        let n = 8 * taps; // long enough for a clean DFT bin
        let skip = 2 * ((taps - 1) / 2); // discard both filter transients
        let x: Vec<f32> = (0..n + 2 * skip)
            .map(|i| (std::f64::consts::TAU * f_hz * i as f64 / fs).cos() as f32)
            .collect();
        let y = HilbertTransformer::with_taps(taps).process(&x);
        let seg = &y[skip..skip + n];
        // Single-bin DFT at +f and -f, Hann-windowed so a non-integer number
        // of cycles in the window cannot masquerade as image energy.
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
        // Sweep the declared band edges and a spread of interior
        // frequencies, both sidebands (the response is symmetric about
        // fs/2).
        let mut f = HILBERT_GUARD_HZ;
        while f <= fs / 2.0 - HILBERT_GUARD_HZ {
            for probe in [f, fs - f] {
                if probe >= fs / 2.0 {
                    continue;
                }
                let r = image_rejection_db(HILBERT_TAPS, probe, fs);
                assert!(
                    r >= HILBERT_MIN_IMAGE_REJECTION_DB,
                    "{probe} Hz: {r:.1} dB image rejection, want >= {HILBERT_MIN_IMAGE_REJECTION_DB}"
                );
            }
            f += 137.0; // deliberately not a channel multiple -- probe off-grid too
        }
    }

    #[test]
    fn the_m1_legacy_design_is_why_man_4_happened() {
        // Regression witness, not a requirement: records *why* 129 taps was
        // replaced. If someone reverts HILBERT_TAPS this test still passes
        // but the contract test above fails, naming the cause.
        let r = image_rejection_db(HILBERT_TAPS_M1_LEGACY, 750.0, 48_000.0);
        assert!(r < 60.0, "legacy 129-tap design measured {r:.1} dB at 750 Hz");
    }

    #[test]
    fn with_taps_rejects_even_lengths() {
        assert!(std::panic::catch_unwind(|| HilbertTransformer::with_taps(128)).is_err());
    }

    #[test]
    fn sparse_tap_evaluation_is_bit_identical_to_dense() {
        // Half the taps are exactly 0.0 (the ideal kernel is zero at every
        // even offset). Skipping them must not perturb the sequential f64
        // accumulation the determinism convention depends on (SPEC 6.4).
        let taps = design_hilbert_fir();
        let center = (taps.len() - 1) / 2;
        // Signed, non-trivial input: exercises the (-0.0)+0.0 edge case.
        let x: Vec<f32> = (0..4096)
            .map(|i| ((i as f32 * 0.017).sin() - 0.5) * if i % 7 == 0 { -1.0 } else { 1.0 })
            .collect();
        let got = HilbertTransformer::new().process(&x);
        // Reference: the dense loop, written out inline.
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
    fn default_design_is_511_taps_with_roughly_half_of_them_structurally_zero() {
        assert_eq!(HILBERT_TAPS, 511);
        let h = design_hilbert_fir();
        // Zero at every even offset from center (511 is odd, so the center
        // tap itself -- offset 0 -- is one of the 255 zero taps; the 256
        // odd offsets are nonzero). Not an exact 255/255 split: an
        // odd-length kernel can't split evenly between two parity classes.
        assert_eq!(h.iter().filter(|t| **t == 0.0).count(), 255);
        assert_eq!(h.iter().filter(|t| **t != 0.0).count(), 256);
    }
}
