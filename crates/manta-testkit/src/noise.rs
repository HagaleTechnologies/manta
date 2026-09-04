//! Complex AWGN with the SNR-in-reference-bandwidth convention.
//! Local stand-in for coppa's future `awgn_ref_bw` (pinned decision 2):
//! coppa-channel's awgn measures duty-cycle-dependent signal power over the
//! full bandwidth and uses a version-unstable RNG — wrong for keyed CW
//! golden vectors. Migrate when coppa ships awgn_ref_bw (SPEC-watterson §6).

use crate::gaussian_pair;
use num_complex::Complex32;
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

/// Add unit-total-power complex white noise (per-component variance 0.5).
/// Sampling order: one Box-Muller pair per sample, (I, Q) — fixed forever.
/// SPEC §7 (deviation: local ref-bw AWGN, pinned decision 2).
pub fn add_unit_awgn(samples: &mut [Complex32], seed: u64) {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let sigma = 0.5f64.sqrt();
    for s in samples.iter_mut() {
        let (zi, zq) = gaussian_pair(&mut rng);
        s.re += (sigma * zi) as f32;
        s.im += (sigma * zq) as f32;
    }
}

/// Key-down carrier amplitude for a requested SNR in 2500 Hz against
/// unit-power noise spread over fs (pinned decision 3; SPEC §7 quotes SNR
/// in 2500 Hz).
pub fn amplitude_for_snr_2500(snr_db: f32, fs: f64) -> f32 {
    ((10.0f64.powf(snr_db as f64 / 10.0)) * 2500.0 / fs).sqrt() as f32
}

/// MAN-4: add real white noise to a real-valued (pre-Hilbert) audio
/// waveform, at the same total-power convention `add_unit_awgn` uses for
/// already-complex IQ (unit total power = 1.0). A Hilbert-transformed real
/// signal's analytic output has total power `2 * Var(real)` (Re and Im
/// each inherit the real signal's own variance -- Hilbert is a unitary,
/// power-preserving transform), so real noise variance `0.5` here becomes
/// exactly unit complex noise power downstream, making
/// `amplitude_for_snr_2500`'s SNR-in-2500-Hz convention hold for tests that
/// build a real waveform and Hilbert-convert it (e.g. `AudioIqSource`
/// fixtures) the same way it already holds for `scene::render_scene`'s
/// complex-IQ path.
pub fn add_real_unit_awgn(samples: &mut [f32], seed: u64) {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let sigma = 0.5f64.sqrt();
    for s in samples.iter_mut() {
        let (z, _) = gaussian_pair(&mut rng);
        *s += (sigma * z) as f32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use num_complex::Complex32;

    #[test]
    fn unit_noise_power_and_determinism() {
        let mut a = vec![Complex32::new(0.0, 0.0); 200_000];
        add_unit_awgn(&mut a, 7);
        let p: f64 = a.iter().map(|c| c.norm_sqr() as f64).sum::<f64>() / a.len() as f64;
        assert!((p - 1.0).abs() < 0.02, "noise power {p}");
        let mut b = vec![Complex32::new(0.0, 0.0); 200_000];
        add_unit_awgn(&mut b, 7);
        assert_eq!(a, b);
    }

    #[test]
    fn real_awgn_matches_unit_power_after_hilbert() {
        // Pre-Hilbert real variance 0.5 -> post-Hilbert analytic total
        // power ~1.0, matching add_unit_awgn's own convention (see this
        // function's doc comment for the derivation).
        let mut real = vec![0.0f32; 200_000];
        add_real_unit_awgn(&mut real, 11);
        let analytic = manta_dsp::hilbert::HilbertTransformer::new().process(&real);
        let p: f64 =
            analytic.iter().map(|c| c.norm_sqr() as f64).sum::<f64>() / analytic.len() as f64;
        assert!((p - 1.0).abs() < 0.05, "post-Hilbert noise power {p}");
    }

    #[test]
    fn amplitude_formula() {
        // +20 dB in 2500 Hz at 96 kS/s: sqrt(100 * 2500/96000) = 1.6137
        let a = amplitude_for_snr_2500(20.0, 96_000.0);
        assert!((a - 1.6137).abs() < 1e-3, "{a}");
    }
}
