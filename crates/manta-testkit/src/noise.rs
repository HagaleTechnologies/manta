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

/// Real-valued analogue of `add_unit_awgn`, for fixtures that drive a real
/// (pre-Hilbert) audio front end such as `manta-input::AudioIqSource`
/// (MAN-273). Adds i.i.d. `N(0, sigma^2)` noise in place. Sampling order:
/// one Box-Muller pair per sample, first value used, its pair-mate
/// discarded -- fixed forever.
pub fn add_real_awgn(samples: &mut [f32], sigma: f32, seed: u64) {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    for s in samples.iter_mut() {
        let (z, _) = gaussian_pair(&mut rng);
        *s += (sigma as f64 * z) as f32;
    }
}

/// The `add_real_awgn` sigma that gives a real, full-scale (amplitude 1.0
/// at key-down) tone the requested SNR in 2500 Hz. A real cosine of
/// amplitude 1 averages power 1/2, and real noise spreads over the
/// one-sided band `fs/2`, so `sigma^2 * (2500 / (fs/2)) = 0.5 / 10^(snr/10)`.
/// Not `amplitude_for_snr_2500`, which scales a complex signal, not noise.
pub fn real_awgn_sigma_for_snr_2500(snr_db: f32, fs: f64) -> f32 {
    let noise_power_2500 = 0.5 / 10.0f64.powf(snr_db as f64 / 10.0);
    (noise_power_2500 / (2500.0 / (fs / 2.0))).sqrt() as f32
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
    fn amplitude_formula() {
        // +20 dB in 2500 Hz at 96 kS/s: sqrt(100 * 2500/96000) = 1.6137
        let a = amplitude_for_snr_2500(20.0, 96_000.0);
        assert!((a - 1.6137).abs() < 1e-3, "{a}");
    }

    #[test]
    fn real_awgn_hits_the_requested_snr_within_half_db() {
        let fs = 48_000.0;
        let n = 200_000;
        let snr_db = 20.0f32;
        let sigma = real_awgn_sigma_for_snr_2500(snr_db, fs);
        let tone: Vec<f32> = (0..n)
            .map(|i| (std::f64::consts::TAU * 750.0 * i as f64 / fs).cos() as f32)
            .collect();
        let tone_power = tone.iter().map(|&x| x as f64 * x as f64).sum::<f64>() / n as f64;
        let mut noise = vec![0.0f32; n];
        add_real_awgn(&mut noise, sigma, 99);
        let noise_power = noise.iter().map(|&x| x as f64 * x as f64).sum::<f64>() / n as f64;
        let noise_power_2500 = noise_power * 2500.0 / (fs / 2.0);
        let achieved_db = 10.0 * (tone_power / noise_power_2500).log10();
        assert!(
            (achieved_db - snr_db as f64).abs() < 0.5,
            "achieved {achieved_db:.2} dB, want {snr_db} dB"
        );
    }

    #[test]
    fn real_awgn_is_deterministic_per_seed() {
        let mut a = vec![0.25f32; 4096];
        let mut b = a.clone();
        add_real_awgn(&mut a, 0.1, 42);
        add_real_awgn(&mut b, 0.1, 42);
        assert_eq!(a, b);
        let mut c = vec![0.25f32; 4096];
        add_real_awgn(&mut c, 0.1, 43);
        assert_ne!(a, c);
    }
}
