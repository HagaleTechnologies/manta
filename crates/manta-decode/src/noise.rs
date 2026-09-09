//! Track-local noise reference: temporal minimum statistics combined with
//! a spectral (guard-banded neighbor) reference. SPEC v2 §2.

use crate::{ms_to_hops, HOP_MS};
use std::collections::VecDeque;

#[derive(Debug, Clone)]
pub struct NoiseConfig {
    pub noise_window_ms: f64,
    /// Bias correction (dB) for the minimum-of-N temporal statistic: the
    /// running min of an exponentially-distributed power sample
    /// underestimates the true mean because it is a minimum, not an
    /// average. Measured empirically (uncorrected `noise_min_bias_db =
    /// 0.0`) for a 1.5 s window (~563 hops) of 40 ms-EMA-smoothed
    /// exponential noise at the 375 Hz hop rate via
    /// `temporal_minimum_is_unbiased_on_noise_within_half_db`: the
    /// uncorrected minimum read -2.159 dB low, so this is set to 2.16 dB
    /// to zero that out (residual after calibration: 0.001 dB). See
    /// `docs/SPEC-decode-core-v2.md` §2.1 for the calibration record.
    pub noise_min_bias_db: f32,
    pub spectral_min_bias_db: f32,
    pub spectral_beta: f32,
    pub tau_ms: f64,
}

impl Default for NoiseConfig {
    fn default() -> Self {
        NoiseConfig {
            noise_window_ms: 1500.0,
            noise_min_bias_db: 2.16,
            spectral_min_bias_db: 1.5,
            spectral_beta: 0.5,
            tau_ms: 40.0,
        }
    }
}

pub struct NoiseTracker {
    cfg: NoiseConfig,
    alpha: f32,
    smoothed: Option<f32>,
    window: usize,
    /// Monotonic deque of (hop, smoothed power): front is the window minimum.
    deque: VecDeque<(u64, f32)>,
    hop: u64,
    b_min: f32,
    b_spec: f32,
}

impl NoiseTracker {
    pub fn new(cfg: NoiseConfig) -> Self {
        let alpha = (1.0 - (-HOP_MS / cfg.tau_ms).exp()) as f32;
        let window = ms_to_hops(cfg.noise_window_ms).max(1) as usize;
        let b_min = 10f32.powf(cfg.noise_min_bias_db / 10.0);
        let b_spec = 10f32.powf(cfg.spectral_min_bias_db / 10.0);
        NoiseTracker {
            cfg,
            alpha,
            smoothed: None,
            window,
            deque: VecDeque::new(),
            hop: 0,
            b_min,
            b_spec,
        }
    }

    /// One hop of the track's own channel power (linear) and, if available,
    /// the raw spectral reference power (linear, from
    /// `FloorBank::spectral_reference_db` converted out of dB). Returns
    /// `N[t]` in linear power. SPEC v2 §2.3.
    pub fn push(&mut self, power: f32, spectral_ref_power: Option<f32>) -> f32 {
        let s = match self.smoothed {
            None => power,
            Some(prev) => prev + self.alpha * (power - prev),
        };
        self.smoothed = Some(s);
        while let Some(&(_, back)) = self.deque.back() {
            if back >= s {
                self.deque.pop_back();
            } else {
                break;
            }
        }
        self.deque.push_back((self.hop, s));
        while let Some(&(h, _)) = self.deque.front() {
            if self.hop - h >= self.window as u64 {
                self.deque.pop_front();
            } else {
                break;
            }
        }
        self.hop += 1;
        let n_temp = self.deque.front().map(|&(_, v)| v).unwrap_or(s) * self.b_min;
        match spectral_ref_power {
            Some(r) => n_temp.max(self.cfg.spectral_beta * self.b_spec * r),
            None => n_temp,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic exponential (chi-square 2 DOF) noise power samples via a
    /// fixed LCG — a test-only RNG, allowed by SPEC v1 §6.1 (seeds are part
    /// of the test).
    fn exp_noise(n: usize, mean: f32, seed: u64) -> Vec<f32> {
        let mut s = seed;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let u = ((s >> 11) as f64 / (1u64 << 53) as f64).max(1e-12);
            out.push((-u.ln() as f32) * mean);
        }
        out
    }

    #[test]
    fn temporal_minimum_is_unbiased_on_noise_within_half_db() {
        let mut t = NoiseTracker::new(NoiseConfig::default());
        let x = exp_noise(375 * 60, 1.0e-6, 7);
        let mut acc = 0.0f64;
        let mut n = 0usize;
        for (i, &p) in x.iter().enumerate() {
            let v = t.push(p, None);
            if i > 375 * 5 {
                acc += v as f64;
                n += 1;
            }
        }
        let est_db = 10.0 * (acc / n as f64 / 1.0e-6).log10();
        assert!(est_db.abs() < 0.5, "temporal noise estimate off by {est_db:.2} dB");
    }

    #[test]
    fn spectral_reference_raises_the_floor_during_a_burst() {
        let mut t = NoiseTracker::new(NoiseConfig::default());
        for _ in 0..375 * 5 {
            t.push(1.0e-6, None);
        }
        let quiet = t.push(1.0e-6, Some(1.0e-6));
        let burst = t.push(1.0e-6, Some(1.0e-3)); // neighbors 30 dB up
        assert!(burst > 100.0 * quiet, "spectral burst must lift N: quiet {quiet:e} burst {burst:e}");
    }
}
