//! **Deprecated** as of M2 sub-project 1 -- `manta-dsp::channelizer`'s
//! real per-channel power output plus its `interpolate_offset` (SPEC §1.4)
//! replace this periodogram-based estimator. Kept compiled and tested for
//! now as a reference/fallback; not wired into `manta-engine` anymore as
//! of that sub-project. Candidate for removal once the channelizer path has
//! run cleanly for a few months; see
//! `docs/DECISIONS/2026-07-18-m2-pfb-channelizer-pins.md`.
//!
//! M0 frequency finder: averaged periodogram + parabolic interpolation.
//! Uses the same dB-domain delta formula and clamp as SPEC §1.4.

use coppa_dsp::fft::FftProcessor;
use num_complex::Complex32;

const FFT_SIZE: usize = 8192;
const FRAME_HOP: usize = FFT_SIZE / 2; // 50 % overlap
const MAX_SECONDS: f64 = 10.0;

/// M0 shim: locates the strongest signal in a wideband IQ scene via an
/// averaged periodogram, so the engine can tune a SingleChannelExtractor to
/// it. Superseded by the PFB detector + track centroid at M2. SPEC §1.3,
/// §1.4.
pub fn estimate_peak_hz(iq: &[Complex32], fs: f64) -> Option<f64> {
    let n_use = iq.len().min((fs * MAX_SECONDS) as usize);
    if n_use < FFT_SIZE {
        return None;
    }
    let fft = FftProcessor::new(FFT_SIZE);
    // Hann window, precomputed in f64, applied in f32.
    let window: Vec<f32> = (0..FFT_SIZE)
        .map(|i| {
            let w =
                0.5 * (1.0 - (2.0 * std::f64::consts::PI * i as f64 / (FFT_SIZE - 1) as f64).cos());
            w as f32
        })
        .collect();

    let mut psd = vec![0.0f64; FFT_SIZE];
    let mut start = 0;
    let mut frames = 0u32;
    let mut buf = vec![Complex32::new(0.0, 0.0); FFT_SIZE];
    while start + FFT_SIZE <= n_use {
        for (b, (x, w)) in buf
            .iter_mut()
            .zip(iq[start..start + FFT_SIZE].iter().zip(&window))
        {
            *b = x * w;
        }
        let spec = fft.forward(&buf);
        for (k, s) in spec.iter().enumerate() {
            psd[k] += s.norm_sqr() as f64;
        }
        frames += 1;
        start += FRAME_HOP;
    }
    debug_assert!(frames > 0);

    // Peak bin: ascending scan, strict greater-than keeps the lowest index on
    // ties (deterministic).
    let mut k0 = 0;
    for (k, &p) in psd.iter().enumerate() {
        if p > psd[k0] {
            k0 = k;
        }
    }
    // Parabolic interpolation on dB powers, SPEC §1.4 formula with clamp.
    let db = |p: f64| 10.0 * (p + 1e-30).log10();
    let pm = db(psd[(k0 + FFT_SIZE - 1) % FFT_SIZE]);
    let p0 = db(psd[k0]);
    let pp = db(psd[(k0 + 1) % FFT_SIZE]);
    let denom = pm - 2.0 * p0 + pp;
    let delta = if denom < 0.0 {
        (0.5 * (pm - pp) / denom).clamp(-0.5, 0.5)
    } else {
        0.0
    };

    // FFT bin order: SPEC §1.3's f(k) = f_center + ((k + N/2) mod N - N/2) * Δ
    // resolves the Nyquist bin (k0 == N/2) to the negative branch, so the
    // wrap threshold here must be `>=`, not `>`.
    let signed_bin = if k0 >= FFT_SIZE / 2 {
        k0 as f64 - FFT_SIZE as f64
    } else {
        k0 as f64
    };
    Some((signed_bin + delta) * fs / FFT_SIZE as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use num_complex::Complex32;

    const FS: f64 = 96_000.0;

    fn tone(freq: f64, n: usize) -> Vec<Complex32> {
        (0..n)
            .map(|i| {
                let phi = 2.0 * std::f64::consts::PI * freq * i as f64 / FS;
                Complex32::new(phi.cos() as f32, phi.sin() as f32)
            })
            .collect()
    }

    #[test]
    fn finds_positive_offset_within_2hz() {
        let est = estimate_peak_hz(&tone(12_340.0, 96_000), FS).unwrap();
        assert!((est - 12_340.0).abs() < 2.0, "est {est}");
    }

    #[test]
    fn finds_negative_offset_within_2hz() {
        let est = estimate_peak_hz(&tone(-20_000.0, 96_000), FS).unwrap();
        assert!((est + 20_000.0).abs() < 2.0, "est {est}");
    }

    #[test]
    fn off_bin_tone_interpolates() {
        // Half-bin offset (bin width 11.72 Hz) is the worst case for the
        // parabolic interpolator.
        let f = 12_340.0 + 5.86;
        let est = estimate_peak_hz(&tone(f, 192_000), FS).unwrap();
        assert!((est - f).abs() < 3.0, "est {est}");
    }

    #[test]
    fn keyed_tone_still_within_3hz() {
        // 50 % duty keying (60 ms period) spreads the line; the average
        // spectrum keeps the carrier dominant.
        let mut iq = tone(12_340.0, 192_000);
        for (i, s) in iq.iter_mut().enumerate() {
            let t_ms = i as f64 * 1000.0 / FS;
            if (t_ms / 60.0) as u64 % 2 == 1 {
                *s = Complex32::new(0.0, 0.0);
            }
        }
        let est = estimate_peak_hz(&iq, FS).unwrap();
        assert!((est - 12_340.0).abs() < 3.0, "est {est}");
    }

    #[test]
    fn too_short_input_is_none() {
        assert!(estimate_peak_hz(&tone(1000.0, 4096), FS).is_none());
    }

    #[test]
    fn deterministic() {
        let iq = tone(12_340.0, 96_000);
        let a = estimate_peak_hz(&iq, FS).unwrap();
        let b = estimate_peak_hz(&iq, FS).unwrap();
        assert_eq!(a.to_bits(), b.to_bits());
    }
}
