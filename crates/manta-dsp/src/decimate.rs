//! Power-of-two decimation stage between an IqSource and the channelizer
//! (issue #169): a cascade of halfband FIR decimate-by-2 stages, letting a
//! capture run at a narrower effective bandwidth than the SDR's native
//! rate while still landing on a channelizer-table-compatible rate
//! (`fs/93.75` a power of two, SPEC §1.1).

use crate::proto::{bessel_i0, KAISER_BETA};
use num_complex::Complex32;

const CHANNEL_SPACING_HZ: f64 = 93.75; // SPEC §1.1, same constant as channelizer.rs/single.rs.

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        let px = std::f64::consts::PI * x;
        px.sin() / px
    }
}

/// Number of taps in each halfband decimate-by-2 stage. Odd length,
/// Kaiser-windowed at the channelizer prototype's 80 dB stopband target
/// (SPEC §1.2's `KAISER_BETA`) so the decimator isn't the weakest link in
/// the alias-rejection chain.
pub const HALFBAND_TAPS: usize = 127;

/// Design one halfband lowpass stage: ideal cutoff at fs_in/4 (the new
/// Nyquist after decimate-by-2), Kaiser-windowed. By construction every
/// even-offset tap except the center is exactly zero (classic halfband
/// property: sinc(k/2) = 0 for even nonzero k).
pub fn design_halfband(taps: usize) -> Vec<f32> {
    assert!(taps % 2 == 1, "halfband design requires odd length");
    let center = (taps - 1) as f64 / 2.0;
    let i0_beta = bessel_i0(KAISER_BETA);
    let mut h = vec![0.0f64; taps];
    let mut sum = 0.0f64;
    for (i, tap) in h.iter_mut().enumerate() {
        let k = i as f64 - center;
        let ideal = 0.5 * sinc(k / 2.0);
        let t = 2.0 * i as f64 / (taps - 1) as f64 - 1.0;
        let w = bessel_i0(KAISER_BETA * (1.0 - t * t).sqrt()) / i0_beta;
        *tap = ideal * w;
        sum += *tap;
    }
    h.iter().map(|&v| (v / sum) as f32).collect()
}

/// One halfband decimate-by-2 stage: direct-form FIR, computed only at
/// kept (every-other) input instants -- never spends a multiply-
/// accumulate on a sample that will be dropped.
///
/// NOTE: does not (yet) skip the ~half of `taps` that are structurally
/// exactly zero (the classic halfband property) -- see
/// `docs/SPEC-decode-core.md` §1.5 and issue #176 for the unrealized 2x
/// MAC saving and the still-missing Pi4 CPU-budget bench for this stage.
struct HalfbandStage {
    taps: Vec<f32>,
    hist: std::collections::VecDeque<Complex32>,
    parity: u64,
}

impl HalfbandStage {
    fn new(taps: Vec<f32>) -> Self {
        let len = taps.len();
        HalfbandStage {
            taps,
            hist: std::collections::VecDeque::from(vec![Complex32::new(0.0, 0.0); len]),
            parity: 0,
        }
    }

    fn process(&mut self, input: &[Complex32]) -> Vec<Complex32> {
        let mut out = Vec::with_capacity(input.len() / 2 + 1);
        for &x in input {
            self.hist.pop_front();
            self.hist.push_back(x);
            let keep = self.parity % 2 == 0;
            self.parity = self.parity.wrapping_add(1);
            if keep {
                // Sequential f64 accumulation (SPEC §6.4 determinism convention).
                let (mut re, mut im) = (0.0f64, 0.0f64);
                for (&h, &s) in self.taps.iter().zip(self.hist.iter()) {
                    re += h as f64 * s.re as f64;
                    im += h as f64 * s.im as f64;
                }
                out.push(Complex32::new(re as f32, im as f32));
            }
        }
        out
    }
}

/// Cascaded halfband decimator: `factor` (a power of two) stages of
/// decimate-by-2, front to back. SPEC-decode-core.md's decimation section.
pub struct Decimator {
    stages: Vec<HalfbandStage>,
    fs_out: f64,
}

impl Decimator {
    /// `factor` must be a power of two, and `fs_in / factor` must itself
    /// satisfy the channelizer's `fs/93.75` power-of-two table constraint
    /// (same validation shape as `Channelizer::new`/
    /// `SingleChannelExtractor::new`) -- a bad target rate fails here, at
    /// construction, not silently downstream. Also rejects a channel count
    /// `n < 4`: `Channelizer::new` sets `hop = n / 4`, which is exactly 0
    /// for `n` in `{1, 2}` (both of which are otherwise valid powers of
    /// two) -- `Channelizer::process`'s `self.read += self.hop` loop then
    /// never advances and never terminates, hanging the process on its
    /// first call. `n == 4` gives `hop == 1`, which advances fine, so `n >=
    /// 4` is the exact safe floor, not merely a conservative guess.
    pub fn new(fs_in: f64, factor: usize) -> Result<Self, String> {
        if factor == 0 || !factor.is_power_of_two() {
            return Err(format!("decimation factor {factor} must be a power of two"));
        }
        let fs_out = fs_in / factor as f64;
        let nf = fs_out / CHANNEL_SPACING_HZ;
        let n = nf.round() as usize;
        if (nf - n as f64).abs() > 1e-9 || !n.is_power_of_two() || n < 4 {
            return Err(format!(
                "unsupported target rate {fs_out}: fs/93.75 must be a power of two >= 4 \
                 (a channelizer with fewer than 4 channels has hop=0 and hangs)"
            ));
        }
        let n_stages = factor.trailing_zeros() as usize;
        let taps = design_halfband(HALFBAND_TAPS);
        let stages = (0..n_stages)
            .map(|_| HalfbandStage::new(taps.clone()))
            .collect();
        Ok(Decimator { stages, fs_out })
    }

    /// The decimated output rate, Hz.
    pub fn fs_out(&self) -> f64 {
        self.fs_out
    }

    /// Feed input IQ at fs_in; returns however many decimated samples
    /// became available (possibly 0, if too few input samples have
    /// accumulated through every cascade stage so far).
    pub fn process(&mut self, iq: &[Complex32]) -> Vec<Complex32> {
        let mut cur = iq.to_vec();
        for stage in &mut self.stages {
            cur = stage.process(&cur);
        }
        cur
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// |H(f)| in dB at frequency f (Hz), direct DTFT -- same helper shape
    /// as proto.rs's `response_db` (test-only, exact not FFT-approximated).
    fn response_db(h: &[f32], f_hz: f64, fs: f64) -> f64 {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, &tap) in h.iter().enumerate() {
            let phi = -2.0 * std::f64::consts::PI * f_hz * i as f64 / fs;
            re += tap as f64 * phi.cos();
            im += tap as f64 * phi.sin();
        }
        10.0 * (re * re + im * im).log10()
    }

    #[test]
    fn halfband_is_symmetric_and_unity_dc() {
        let h = design_halfband(HALFBAND_TAPS);
        assert_eq!(h.len(), HALFBAND_TAPS);
        for i in 0..h.len() / 2 {
            assert_eq!(h[i], h[h.len() - 1 - i], "tap {i} asymmetric");
        }
        let sum: f64 = h.iter().map(|&x| x as f64).sum();
        assert!((sum - 1.0).abs() < 1e-6, "DC gain {sum}");
    }

    #[test]
    fn halfband_even_offset_taps_are_zero() {
        // Classic halfband property: every tap at an even offset from the
        // center (except the center itself) is exactly zero.
        let h = design_halfband(HALFBAND_TAPS);
        let center = (h.len() - 1) / 2;
        for k in (2..center).step_by(2) {
            assert!(
                h[center + k].abs() < 1e-6,
                "tap at +{k} not ~0: {}",
                h[center + k]
            );
            assert!(
                h[center - k].abs() < 1e-6,
                "tap at -{k} not ~0: {}",
                h[center - k]
            );
        }
    }

    #[test]
    fn halfband_stopband_at_least_78_db() {
        // fs = 192 kHz -> new Nyquist (cutoff) at 48 kHz. Check well past
        // the cutoff, out to the original Nyquist (96 kHz).
        let fs = 192_000.0;
        let h = design_halfband(HALFBAND_TAPS);
        let mut worst = -300.0f64;
        let mut f = 60_000.0; // guard band above the 48 kHz cutoff
        while f < 96_000.0 {
            worst = worst.max(response_db(&h, f, fs));
            f += 500.0;
        }
        assert!(
            worst <= -78.0,
            "worst stopband {worst} dB (raise HALFBAND_TAPS if this fails)"
        );
    }

    #[test]
    fn halfband_passband_is_flat_near_dc() {
        let fs = 192_000.0;
        let h = design_halfband(HALFBAND_TAPS);
        // Well inside the 48 kHz cutoff; DC-normalized gain should sit
        // close to 0 dB.
        let gain_db = response_db(&h, 5_000.0, fs);
        assert!(gain_db.abs() < 0.5, "passband gain {gain_db} dB");
    }

    fn tone(freq: f64, n: usize, amp: f32, fs: f64) -> Vec<Complex32> {
        (0..n)
            .map(|i| {
                let phi = 2.0 * std::f64::consts::PI * freq * i as f64 / fs;
                Complex32::new(amp * phi.cos() as f32, amp * phi.sin() as f32)
            })
            .collect()
    }

    #[test]
    fn rejects_non_power_of_two_factor() {
        assert!(Decimator::new(192_000.0, 3).is_err());
        assert!(Decimator::new(192_000.0, 4).is_ok());
    }

    #[test]
    fn rejects_target_rate_failing_channelizer_table_constraint() {
        // fs_in=100_000, factor=2 (a power of two) -> fs_out=50_000, whose
        // fs/93.75 = 533.33.. is not an integer, let alone a power of two.
        assert!(Decimator::new(100_000.0, 2).is_err());
    }

    #[test]
    fn rejects_degenerate_channel_count_that_would_hang_the_channelizer() {
        // fs_in=48_000, factor=256 (a power of two) -> fs_out=187.5, whose
        // fs/93.75 = 2 IS a power of two, so the old check alone would
        // accept this -- but Channelizer::new's hop = n/4 = 0 for n=2,
        // which hangs Channelizer::process's read-advancing loop forever.
        // Must be rejected at construction, not hang the process later.
        assert!(Decimator::new(48_000.0, 256).is_err());
        // n=1 (fs_out=93.75) is equally degenerate.
        assert!(Decimator::new(375.0, 4).is_err());
        // n=4 (fs_out=375, hop=1) is the smallest table size that is safe.
        assert!(Decimator::new(1_500.0, 4).is_ok());
    }

    #[test]
    fn fs_out_matches_factor() {
        let d = Decimator::new(192_000.0, 4).unwrap();
        assert_eq!(d.fs_out(), 48_000.0);
    }

    #[test]
    fn dc_tone_passes_through_decimation_at_near_unity() {
        let fs_in = 192_000.0;
        let mut d = Decimator::new(fs_in, 4).unwrap();
        // A tone well inside the final 48 kHz/2=24 kHz Nyquist.
        let iq = tone(2_000.0, 20_000, 1.0, fs_in);
        let out = d.process(&iq);
        // Skip transient: two cascaded HALFBAND_TAPS-length filters need
        // roughly HALFBAND_TAPS output samples (post-decimation) to settle.
        let warmup = HALFBAND_TAPS;
        assert!(out.len() > warmup + 50, "only {} output samples", out.len());
        for s in &out[warmup..] {
            assert!((s.norm() - 1.0).abs() < 0.1, "mag {}", s.norm());
        }
    }

    #[test]
    fn tone_above_new_nyquist_is_rejected() {
        let fs_in = 192_000.0;
        let mut d = Decimator::new(fs_in, 4).unwrap();
        // 30 kHz is above the final 24 kHz Nyquist -- must be suppressed,
        // not aliased through as a spurious in-band tone.
        let iq = tone(30_000.0, 20_000, 1.0, fs_in);
        let out = d.process(&iq);
        let warmup = HALFBAND_TAPS;
        for s in &out[warmup..] {
            assert!(s.norm() < 0.05, "aliased mag {}", s.norm());
        }
    }

    #[test]
    fn is_deterministic() {
        let fs_in = 192_000.0;
        let iq = tone(2_000.0, 40_000, 1.0, fs_in);
        let mut d_a = Decimator::new(fs_in, 4).unwrap();
        let mut d_b = Decimator::new(fs_in, 4).unwrap();
        let out_a = d_a.process(&iq);
        let out_b = d_b.process(&iq);
        assert_eq!(out_a.len(), out_b.len());
        for (a, b) in out_a.iter().zip(out_b.iter()) {
            assert_eq!(a.re.to_bits(), b.re.to_bits());
            assert_eq!(a.im.to_bits(), b.im.to_bits());
        }
    }

    #[test]
    fn process_across_multiple_calls_matches_one_call() {
        let fs_in = 192_000.0;
        let iq = tone(2_000.0, 20_000, 1.0, fs_in);
        let mut whole = Decimator::new(fs_in, 4).unwrap();
        let out_whole = whole.process(&iq);

        let mut chunked = Decimator::new(fs_in, 4).unwrap();
        let mut out_chunked = Vec::new();
        for chunk in iq.chunks(137) {
            out_chunked.extend(chunked.process(chunk));
        }
        assert_eq!(out_whole.len(), out_chunked.len());
        for (a, b) in out_whole.iter().zip(out_chunked.iter()) {
            assert_eq!(a.re.to_bits(), b.re.to_bits());
            assert_eq!(a.im.to_bits(), b.im.to_bits());
        }
    }
}
