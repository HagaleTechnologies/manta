//! Real-to-analytic (Hilbert) conversion: odd-length windowed-sinc FIR,
//! Kaiser-windowed identically to the PFB prototype (proto.rs). Used both
//! for live audio input (skimmer-input::AudioIqSource) and offline
//! Watterson vector rendering (skimmer-testkit). Design doc §3.

use crate::proto::{bessel_i0, KAISER_BETA};
use num_complex::Complex32;

/// Hilbert FIR length (odd). 129 taps gives a well-behaved passband from a
/// few hundred Hz to several kHz at 48 kHz -- comfortably covers rig audio
/// and the CW tone offsets M1 uses.
pub const HILBERT_TAPS: usize = 129;

/// Design the length-HILBERT_TAPS windowed-sinc Hilbert FIR:
/// h\[n\] = 0 for (n - center) even, -2 / (pi * (n - center)) for odd,
/// Kaiser-windowed with the PFB prototype's beta (proto.rs). The negative
/// sign is required because `process()` pairs `taps[i]` directly with
/// `hist[i]` (oldest-first), which reverses the convolution index order
/// relative to standard causal FIR. Since the ideal Hilbert kernel is
/// antisymmetric, this index reversal is algebraically equivalent to
/// negating the kernel.
pub fn design_hilbert_fir() -> Vec<f32> {
    let len = HILBERT_TAPS;
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

/// Streaming FIR Hilbert transformer: incrementally converts real samples
/// to an analytic (I = delayed real, Q = Hilbert-filtered) signal. Causal,
/// fixed group delay of (HILBERT_TAPS-1)/2 samples, callable across
/// multiple `process` calls with persistent history (design doc §3).
pub struct HilbertTransformer {
    taps: Vec<f32>,
    /// Ring of the last HILBERT_TAPS real input samples, oldest first.
    hist: std::collections::VecDeque<f32>,
}

impl Default for HilbertTransformer {
    fn default() -> Self {
        Self::new()
    }
}

impl HilbertTransformer {
    pub fn new() -> Self {
        HilbertTransformer {
            taps: design_hilbert_fir(),
            hist: std::collections::VecDeque::from(vec![0.0f32; HILBERT_TAPS]),
        }
    }

    /// Fixed causal group delay, in samples: (HILBERT_TAPS - 1) / 2.
    pub fn delay(&self) -> usize {
        (HILBERT_TAPS - 1) / 2
    }

    /// Convert one chunk of real samples to analytic Complex32 samples, one
    /// output per input, using a persistent history window across calls.
    pub fn process(&mut self, input: &[f32]) -> Vec<Complex32> {
        let center = self.delay();
        let mut out = Vec::with_capacity(input.len());
        for &x in input {
            self.hist.pop_front();
            self.hist.push_back(x);
            // Sequential f64 accumulation (SPEC §6.4 determinism convention).
            let mut acc = 0.0f64;
            for (i, &h) in self.taps.iter().enumerate() {
                acc += h as f64 * self.hist[i] as f64;
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
        let center = (HILBERT_TAPS - 1) / 2;
        assert_eq!(h[center], 0.0, "center tap (k=0) must be exactly zero");
        assert_eq!(h[center + 2], 0.0, "k=+2 (even) must be exactly zero");
        assert_eq!(h[center - 2], 0.0, "k=-2 (even) must be exactly zero");
        assert!(h[center + 1] != 0.0, "k=+1 (odd) must be nonzero");
    }

    #[test]
    fn fir_is_antisymmetric() {
        // Ideal Hilbert kernel h[n] = 2/(pi*n) is odd: h[center+k] = -h[center-k].
        let h = design_hilbert_fir();
        let center = (HILBERT_TAPS - 1) / 2;
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
}
