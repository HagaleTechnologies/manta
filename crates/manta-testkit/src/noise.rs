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

/// Add real-valued white Gaussian noise (per-sample variance `sigma^2`) to
/// a real audio buffer -- the real-domain counterpart to `add_unit_awgn`,
/// for fixtures that model a real soundcard/microphone front end
/// (`manta_input::AudioIqSource`) rather than a source that already
/// supplies complex IQ. MAN-4: a strictly noiseless real-audio fixture has
/// no floor for `manta-dsp::floor::FloorBank`'s percentile estimator to
/// find (every silent channel clamps to the histogram minimum), so
/// keying-edge transient splatter that would be many dB under a real
/// receiver's noise floor instead reads as tens of dB above it and spawns
/// spurious tracks -- see `docs/DECISIONS/2026-09-04-man-4-hilbert-guard-pins.md`.
pub fn add_real_awgn(samples: &mut [f32], sigma: f32, seed: u64) {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    for s in samples.iter_mut() {
        let (z, _) = gaussian_pair(&mut rng);
        *s += (z * sigma as f64) as f32;
    }
}

/// Real-audio-domain analog of `amplitude_for_snr_2500`: the per-sample
/// standard deviation of `add_real_awgn` noise that gives a unit-peak-
/// amplitude real tone (average power 1/2) the requested SNR in a 2500 Hz
/// reference bandwidth, at sample rate `fs`. Real white noise of variance
/// `sigma^2` sampled at `fs` spreads that power over the one-sided
/// bandwidth `fs/2` (unlike `add_unit_awgn`'s complex noise, which spreads
/// over the full `fs`), so the derivation differs from
/// `amplitude_for_snr_2500` by that factor of 2 as well as the 1/2
/// key-down-power term.
pub fn real_noise_sigma_for_snr_2500(snr_db: f32, fs: f64) -> f32 {
    let snr = 10.0f64.powf(snr_db as f64 / 10.0);
    (0.5 * fs / (5000.0 * snr)).sqrt() as f32
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
    fn real_noise_power_and_determinism() {
        let mut a = vec![0.0f32; 200_000];
        add_real_awgn(&mut a, 2.0, 7);
        let p: f64 = a.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / a.len() as f64;
        assert!((p - 4.0).abs() < 0.1, "noise power {p}, expected sigma^2=4.0");
        let mut b = vec![0.0f32; 200_000];
        add_real_awgn(&mut b, 2.0, 7);
        assert_eq!(a, b);
    }

    #[test]
    fn real_sigma_formula() {
        // +30 dB in 2500 Hz at 48 kS/s: sqrt(0.5*48000/(5000*1000)) = 0.069282...
        let s = real_noise_sigma_for_snr_2500(30.0, 48_000.0);
        assert!((s - 0.069282).abs() < 1e-5, "{s}");
    }
}
