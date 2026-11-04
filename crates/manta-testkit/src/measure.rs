//! Independent measurement of physical signal properties (MAN-206).
//!
//! The cross-check gate (`crosscheck`) compares two generators through these
//! estimators, so this module shares no code with any generator or with the
//! decoder: its own radix-2 FFT, its own Welch PSD, its own run finder. Each
//! estimator is checked against a closed form in this module's tests.

use num_complex::{Complex32, Complex64};

/// In-place iterative radix-2 forward DFT (`X[k] = sum x[n] e^{-j2πkn/N}`).
/// `x.len()` must be a power of two.
pub fn fft(x: &mut [Complex64]) {
    let n = x.len();
    assert!(n.is_power_of_two(), "fft length {n} is not a power of two");
    if n < 2 {
        return;
    }
    let bits = n.trailing_zeros();
    for i in 0..n {
        let j = i.reverse_bits() >> (usize::BITS - bits);
        if j > i {
            x.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -std::f64::consts::TAU / len as f64;
        let half = len / 2;
        let tw: Vec<Complex64> = (0..half)
            .map(|k| Complex64::from_polar(1.0, ang * k as f64))
            .collect();
        for start in (0..n).step_by(len) {
            for k in 0..half {
                let a = x[start + k];
                let b = x[start + k + half] * tw[k];
                x[start + k] = a + b;
                x[start + k + half] = a - b;
            }
        }
        len <<= 1;
    }
}

/// A two-sided power spectral density estimate, bins in FFT order.
#[derive(Debug, Clone)]
pub struct Psd {
    pub fs: f64,
    /// Power per Hz in each bin; `sum(bins) * bin_width()` is mean power.
    pub bins: Vec<f64>,
}

impl Psd {
    pub fn bin_width(&self) -> f64 {
        self.fs / self.bins.len() as f64
    }

    /// Signed centre frequency of bin `k`, in `[-fs/2, fs/2)`.
    pub fn freq(&self, k: usize) -> f64 {
        let n = self.bins.len();
        let k = if k < n / 2 {
            k as f64
        } else {
            k as f64 - n as f64
        };
        k * self.bin_width()
    }

    /// Power in bins whose centre lies in `[lo, hi]` Hz.
    pub fn band_power(&self, lo: f64, hi: f64) -> f64 {
        let df = self.bin_width();
        (0..self.bins.len())
            .filter(|&k| {
                let f = self.freq(k);
                f >= lo && f <= hi
            })
            .map(|k| self.bins[k] * df)
            .sum()
    }

    /// Total power, `sum(bins) * bin_width()`.
    pub fn total_power(&self) -> f64 {
        self.bins.iter().sum::<f64>() * self.bin_width()
    }

    /// Indices of every bin within `db` of the peak bin.
    pub fn bins_within_db_of_peak(&self, db: f64) -> Vec<usize> {
        let peak = self.bins.iter().copied().fold(0.0f64, f64::max);
        let floor = peak * 10f64.powf(-db / 10.0);
        (0..self.bins.len())
            .filter(|&k| self.bins[k] >= floor && self.bins[k] > 0.0)
            .collect()
    }
}

/// Welch PSD: Hann window, 50 % overlap, `nfft` a power of two. Scaled so
/// that `Psd::total_power` equals the mean power of the windowed segments
/// (the mean power of `x` for a stationary input).
pub fn welch_psd(x: &[Complex32], fs: f64, nfft: usize) -> Psd {
    let nfft = nfft.min(x.len().next_power_of_two()).max(2);
    let win: Vec<f64> = (0..nfft)
        .map(|i| {
            let s = (std::f64::consts::PI * i as f64 / nfft as f64).sin();
            s * s
        })
        .collect();
    let wpow: f64 = win.iter().map(|w| w * w).sum();
    let hop = nfft / 2;
    let mut acc = vec![0.0f64; nfft];
    let mut segments = 0usize;
    let mut buf = vec![Complex64::new(0.0, 0.0); nfft];
    let mut start = 0usize;
    while start + nfft <= x.len() || (segments == 0 && start == 0) {
        for (i, b) in buf.iter_mut().enumerate() {
            let s = x
                .get(start + i)
                .map(|c| Complex64::new(c.re as f64, c.im as f64))
                .unwrap_or_default();
            *b = s * win[i];
        }
        fft(&mut buf);
        for (a, b) in acc.iter_mut().zip(buf.iter()) {
            *a += b.norm_sqr();
        }
        segments += 1;
        start += hop;
        if start + nfft > x.len() {
            break;
        }
    }
    let scale = 1.0 / (segments as f64 * fs * wpow);
    Psd {
        fs,
        bins: acc.into_iter().map(|a| a * scale).collect(),
    }
}

/// Mean power `mean(|x|^2)`.
pub fn total_power(x: &[Complex32]) -> f64 {
    if x.is_empty() {
        return 0.0;
    }
    x.iter().map(|c| c.norm_sqr() as f64).sum::<f64>() / x.len() as f64
}

/// Power in `[-f - half_band, -f + half_band]` over power in
/// `[f - half_band, f + half_band]`, in dB: the mirror image of a tone at
/// `f` relative to the tone.
pub fn image_dbc(x: &[Complex32], f: f64, fs: f64, half_band: f64) -> f64 {
    let psd = welch_psd(x, fs, 1 << 14);
    let main = psd.band_power(f - half_band, f + half_band);
    let image = psd.band_power(-f - half_band, -f + half_band);
    10.0 * (image.max(1e-300) / main.max(1e-300)).log10()
}

/// Runs of `mag` at or above `frac_of_plateau` times its maximum, as
/// `(first_sample, length)`.
pub fn on_runs(mag: &[f64], frac_of_plateau: f64) -> Vec<(usize, usize)> {
    let plateau = mag.iter().copied().fold(0.0f64, f64::max);
    let thr = plateau * frac_of_plateau;
    let mut runs = Vec::new();
    let mut start: Option<usize> = None;
    for (i, &m) in mag.iter().enumerate() {
        let on = plateau > 0.0 && m >= thr;
        match (on, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                runs.push((s, i - s));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        runs.push((s, mag.len() - s));
    }
    runs
}

/// Lag in `0..=max_lag` samples at which `delayed` best matches
/// `reference` (peak of the cross-correlation magnitude over the first
/// 32768 samples).
pub fn group_delay_samples(reference: &[Complex32], delayed: &[Complex32], max_lag: usize) -> i64 {
    let n = reference.len().min(delayed.len()).min(1 << 15);
    let mut best = (0usize, f64::MIN);
    for lag in 0..=max_lag.min(n.saturating_sub(1)) {
        let c: Complex64 = (0..n - lag)
            .map(|i| {
                let a = delayed[i + lag];
                let b = reference[i];
                Complex64::new(a.re as f64, a.im as f64) * Complex64::new(b.re as f64, -b.im as f64)
            })
            .sum();
        if c.norm() > best.1 {
            best = (lag, c.norm());
        }
    }
    best.0 as i64
}

/// SNR in the 2500 Hz reference bandwidth: key-down power of `clean`
/// (samples at or above 0.999 of its plateau magnitude) over the noise PSD
/// of `noise_only` integrated across 2500 Hz centred on `f0`.
pub fn snr_2500_db(clean: &[Complex32], noise_only: &[Complex32], fs: f64, f0: f64) -> f64 {
    let plateau = clean.iter().map(|c| c.norm() as f64).fold(0.0f64, f64::max);
    let keydown: Vec<f64> = clean
        .iter()
        .map(|c| c.norm_sqr() as f64)
        .filter(|&p| p.sqrt() >= 0.999 * plateau)
        .collect();
    let p_sig = keydown.iter().sum::<f64>() / keydown.len().max(1) as f64;
    let psd = welch_psd(noise_only, fs, 1 << 12);
    let p_noise = psd.band_power(f0 - 1250.0, f0 + 1250.0);
    10.0 * (p_sig / p_noise).log10()
}

/// Normalized autocorrelation of complex gain processes at `lags` (samples),
/// pooled over realizations: numerator averaged over the overlap length at
/// each lag, denominator the mean power. Real part.
pub fn fade_autocorr(gains: &[Vec<Complex64>], lags: &[usize]) -> Vec<f64> {
    let den: f64 = gains
        .iter()
        .map(|g| g.iter().map(|c| c.norm_sqr()).sum::<f64>() / g.len() as f64)
        .sum::<f64>()
        / gains.len() as f64;
    lags.iter()
        .map(|&lag| {
            let num: f64 = gains
                .iter()
                .map(|g| {
                    let m = g.len().saturating_sub(lag).max(1);
                    (0..g.len().saturating_sub(lag))
                        .map(|i| (g[i + lag] * g[i].conj()).re)
                        .sum::<f64>()
                        / m as f64
                })
                .sum::<f64>()
                / gains.len() as f64;
            num / den
        })
        .collect()
}

/// Mean over realizations of each realization's mean power, and the
/// standard error of that mean.
pub fn ensemble_power(gains: &[Vec<Complex64>]) -> (f64, f64) {
    let p: Vec<f64> = gains
        .iter()
        .map(|g| g.iter().map(|c| c.norm_sqr()).sum::<f64>() / g.len() as f64)
        .collect();
    let k = p.len() as f64;
    let mean = p.iter().sum::<f64>() / k;
    let var = if p.len() > 1 {
        p.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (k - 1.0)
    } else {
        0.0
    };
    (mean, (var / k).sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_chacha::ChaCha8Rng;
    use rand_core::{Rng, SeedableRng};

    fn uniform(rng: &mut ChaCha8Rng) -> f64 {
        (rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn white(n: usize, seed: u64) -> Vec<Complex32> {
        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        (0..n)
            .map(|_| loop {
                let u = 2.0 * uniform(&mut rng) - 1.0;
                let v = 2.0 * uniform(&mut rng) - 1.0;
                let s = u * u + v * v;
                if s > 0.0 && s < 1.0 {
                    let m = (-2.0 * s.ln() / s).sqrt() * 0.5f64.sqrt();
                    break Complex32::new((u * m) as f32, (v * m) as f32);
                }
            })
            .collect()
    }

    fn tone(n: usize, a: f64, f: f64, fs: f64) -> Vec<Complex32> {
        (0..n)
            .map(|i| {
                let c = Complex64::from_polar(a, std::f64::consts::TAU * f * i as f64 / fs);
                Complex32::new(c.re as f32, c.im as f32)
            })
            .collect()
    }

    #[test]
    fn fft_matches_direct_dft() {
        let mut rng = ChaCha8Rng::seed_from_u64(3);
        let x: Vec<Complex64> = (0..64)
            .map(|_| Complex64::new(uniform(&mut rng) - 0.5, uniform(&mut rng) - 0.5))
            .collect();
        let mut y = x.clone();
        fft(&mut y);
        for (k, yk) in y.iter().enumerate() {
            let d: Complex64 = x
                .iter()
                .enumerate()
                .map(|(i, xi)| {
                    xi * Complex64::from_polar(1.0, -std::f64::consts::TAU * (k * i) as f64 / 64.0)
                })
                .sum();
            assert!((d - yk).norm() < 1e-9, "bin {k}: {d} vs {yk}");
        }
    }

    #[test]
    fn welch_psd_integrates_to_mean_power() {
        let fs = 96_000.0;
        let x = white(1 << 17, 5);
        let psd = welch_psd(&x, fs, 1 << 12);
        let db = 10.0 * (psd.total_power() / total_power(&x)).log10();
        assert!(db.abs() < 0.01, "white noise: {db} dB");
        let t = tone(1 << 16, 1.3, 12_000.0, fs);
        let psd = welch_psd(&t, fs, 1 << 12);
        let db = 10.0 * (psd.total_power() / total_power(&t)).log10();
        assert!(db.abs() < 0.01, "tone: {db} dB");
    }

    #[test]
    fn tone_band_power_is_amplitude_squared() {
        let fs = 96_000.0;
        let a = 1.6137;
        let t = tone(1 << 17, a, 12_340.0, fs);
        let p = welch_psd(&t, fs, 1 << 14).band_power(12_240.0, 12_440.0);
        let db = 10.0 * (p / (a * a)).log10();
        assert!(db.abs() < 0.01, "{db} dB");
    }

    #[test]
    fn image_dbc_of_a_pure_complex_tone_is_below_minus_140() {
        let t = tone(1 << 16, 1.0, 5_000.0, 96_000.0);
        let i = image_dbc(&t, 5_000.0, 96_000.0, 100.0);
        assert!(i < -140.0, "{i}");
    }

    #[test]
    fn image_dbc_of_a_cosine_is_zero() {
        let fs = 96_000.0;
        let c: Vec<Complex32> = (0..1 << 16)
            .map(|i| {
                Complex32::new(
                    (std::f64::consts::TAU * 5_000.0 * i as f64 / fs).cos() as f32,
                    0.0,
                )
            })
            .collect();
        let i = image_dbc(&c, 5_000.0, fs, 100.0);
        assert!(i.abs() < 0.01, "{i}");
    }

    #[test]
    fn on_runs_finds_rectangular_pulses_exactly() {
        let mut m = vec![0.0f64; 100];
        for v in &mut m[10..20] {
            *v = 1.0;
        }
        for v in &mut m[50..77] {
            *v = 1.0;
        }
        m[99] = 1.0;
        assert_eq!(on_runs(&m, 0.5), vec![(10, 10), (50, 27), (99, 1)]);
    }

    #[test]
    fn group_delay_of_a_shifted_pulse_is_exact() {
        let t = tone(4000, 1.0, 3_000.0, 96_000.0);
        let mut x: Vec<Complex32> = vec![Complex32::new(0.0, 0.0); 4000];
        x[1000..3000].copy_from_slice(&t[1000..3000]);
        let mut y = vec![Complex32::new(0.0, 0.0); 4000];
        y[1064..].copy_from_slice(&x[1000..3936]);
        y[..1064].copy_from_slice(
            &x[..1064]
                .iter()
                .map(|_| Complex32::new(0.0, 0.0))
                .collect::<Vec<_>>(),
        );
        assert_eq!(group_delay_samples(&x, &y, 200), 64);
    }

    #[test]
    fn snr_2500_of_a_constructed_scene() {
        let fs = 96_000.0;
        let p: f64 = 0.25;
        let clean = tone(1 << 18, p.sqrt(), 10_000.0, fs);
        let noise = white(1 << 18, 11);
        let got = snr_2500_db(&clean, &noise, fs, 10_000.0);
        let want = 10.0 * (p / (1.0 / fs * 2500.0)).log10();
        assert!((got - want).abs() < 0.1, "got {got}, want {want}");
    }

    #[test]
    fn fade_autocorr_of_a_known_two_tone_gain() {
        // g = (e^{j2π f1 t} + e^{j2π f2 t}) / sqrt(2): R(τ) = cos(πΔf τ)·
        // phase term; with f1 = -f2 the real part is cos(2π f1 τ).
        let rate = 1000.0;
        let f1 = 0.37;
        let g: Vec<Complex64> = (0..60_000)
            .map(|i| {
                let t = i as f64 / rate;
                (Complex64::from_polar(1.0, std::f64::consts::TAU * f1 * t)
                    + Complex64::from_polar(1.0, -std::f64::consts::TAU * f1 * t))
                    / 2f64.sqrt()
            })
            .collect();
        let lags = [100usize, 500, 1000];
        let r = fade_autocorr(std::slice::from_ref(&g), &lags);
        for (lag, got) in lags.iter().zip(r) {
            let want = (std::f64::consts::TAU * f1 * *lag as f64 / rate).cos();
            assert!((got - want).abs() < 1e-3, "lag {lag}: {got} vs {want}");
        }
    }

    #[test]
    fn overlap_normalization_is_unbiased() {
        // A constant gain has R(τ) = 1 at every lag; normalizing by the full
        // length instead of the overlap would give (n - τ)/n.
        let g = vec![Complex64::new(0.6, 0.8); 1000];
        let r = fade_autocorr(std::slice::from_ref(&g), &[0, 500, 900]);
        for v in r {
            assert!((v - 1.0).abs() < 1e-12, "{v}");
        }
    }

    #[test]
    fn ensemble_power_reports_mean_and_standard_error() {
        let gains: Vec<Vec<Complex64>> = [1.0f64, 2.0, 3.0]
            .iter()
            .map(|&p| vec![Complex64::new(p.sqrt(), 0.0); 10])
            .collect();
        let (m, se) = ensemble_power(&gains);
        assert!((m - 2.0).abs() < 1e-12);
        assert!((se - (1.0f64 / 3.0).sqrt()).abs() < 1e-12, "{se}");
    }
}
