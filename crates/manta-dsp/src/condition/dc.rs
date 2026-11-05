//! DC removal (MAN-203 D5): a first-order DC blocker with a 5 Hz corner.
//!
//! The estimate `dc` starts at zero and updates once per sample:
//! `y[n] = x[n] - dc[n]`, then `dc[n+1] = dc[n] + a_n * clamp(y[n])`, with
//! `a_n = max(2π·DC_CORNER_HZ/fs, 1/(n+1))` (a running mean at the start,
//! so the estimate settles fast) and `clamp` limiting the update's
//! magnitude to [`DC_UPDATE_CLAMP`] (full scale, SPEC §1.3), so a stray
//! impulse moves the estimate by at most `a_n`. The update reads only
//! the f32 output `y`, so [`revert_dc`] rebuilds every `dc[n]` from the
//! corrected stream and restores `x[n] = y[n] + dc[n]` exactly up to the
//! f32 rounding of `y`. State is f64. Exact-zero samples before the first
//! nonzero one (lead-in padding) do not advance the state, so `n` counts
//! from the first real sample and reverting the stream with or without
//! its leading zeros gives the same result.

use num_complex::{Complex32, Complex64};

/// The blocker's corner frequency, Hz. A constant, not a config key: 2, 5
/// and 20 Hz were indistinguishable in the MAN-203 research, and a station
/// within about 40 Hz of the centre is undecodable anyway.
pub const DC_CORNER_HZ: f64 = 5.0;

/// The largest magnitude one sample's update may move the estimate by
/// before the `a_n` weight (full scale).
pub const DC_UPDATE_CLAMP: f64 = 1.0;

/// The DC blocker's state.
#[derive(Debug, Clone)]
pub struct DcBlocker {
    a: f64,
    n: u64,
    dc: Complex64,
}

impl DcBlocker {
    /// A blocker for sample rate `fs`, with a zero estimate.
    pub fn new(fs: f64) -> Self {
        DcBlocker {
            a: std::f64::consts::TAU * DC_CORNER_HZ / fs,
            n: 0,
            dc: Complex64::new(0.0, 0.0),
        }
    }

    /// The current DC estimate (the value the next sample will have
    /// subtracted).
    pub fn level(&self) -> Complex64 {
        self.dc
    }

    /// Returns `x` minus the current estimate, and advances the estimate.
    pub fn process_sample(&mut self, x: Complex32) -> Complex32 {
        let y64 = Complex64::new(x.re as f64, x.im as f64) - self.dc;
        let y = Complex32::new(y64.re as f32, y64.im as f32);
        self.advance(y);
        y
    }

    /// Advances the estimate from one corrected output sample. Exact-zero
    /// samples before the first nonzero one (the engine's lead-in padding)
    /// leave the state untouched, so the fast start begins at the first
    /// real sample. The rule reads only `y` and the state, so
    /// [`revert_dc`] applies it identically.
    fn advance(&mut self, y: Complex32) {
        if self.n == 0 && y.re == 0.0 && y.im == 0.0 {
            return;
        }
        let a = self.a.max(1.0 / (self.n as f64 + 1.0));
        let mut innov = Complex64::new(y.re as f64, y.im as f64);
        let mag = innov.norm();
        if mag > DC_UPDATE_CLAMP {
            innov *= DC_UPDATE_CLAMP / mag;
        }
        self.dc += innov * a;
        self.n += 1;
    }
}

/// The inverse of an applied DC blocker that started at the stream's first
/// sample: rebuilds the estimate from the corrected samples `y` and returns
/// the original `x[n] = y[n] + dc[n]`.
pub fn revert_dc(y: &[Complex32], fs: f64) -> Vec<Complex32> {
    let mut b = DcBlocker::new(fs);
    y.iter()
        .map(|&yn| {
            let x = Complex64::new(yn.re as f64, yn.im as f64) + b.dc;
            b.advance(yn);
            Complex32::new(x.re as f32, x.im as f32)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 96_000.0;

    /// A deterministic, roughly Gaussian test signal (no testkit here).
    fn noise(n: usize, seed: u64, rms: f32) -> Vec<Complex32> {
        let mut s = seed;
        let mut u = move || {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((s >> 11) as f64 / (1u64 << 53) as f64) as f32 - 0.5
        };
        (0..n)
            .map(|_| {
                let re: f32 = (0..6).map(|_| u()).sum();
                let im: f32 = (0..6).map(|_| u()).sum();
                Complex32::new(re, im) * rms
            })
            .collect()
    }

    fn apply(x: &[Complex32]) -> Vec<Complex32> {
        let mut b = DcBlocker::new(FS);
        x.iter().map(|&s| b.process_sample(s)).collect()
    }

    #[test]
    fn dc_blocker_reverts_exactly() {
        let mut x = noise(2 * FS as usize, 7, 0.05);
        for s in x.iter_mut() {
            *s += Complex32::new(0.01, 0.007);
        }
        // An impulse of 1e9 hits the update clamp.
        x[50_000] = Complex32::new(1e9, -1e9);
        let y = apply(&x);
        let back = revert_dc(&y, FS);
        for (i, (a, b)) in x.iter().zip(&back).enumerate() {
            let tol = 1e-6 * a.norm().max(1e-2);
            assert!((a - b).norm() <= tol, "sample {i}: {a} vs {b}");
        }
    }

    #[test]
    fn leading_zeros_keep_the_fast_start_and_the_inverse() {
        let pad = 8192;
        let mut x = vec![Complex32::new(0.0, 0.0); pad];
        let mut tail = noise(FS as usize / 4, 5, 0.05);
        for s in tail.iter_mut() {
            *s += Complex32::new(0.01, 0.007);
        }
        x.extend_from_slice(&tail);
        let y = apply(&x);
        // The padding passes through as zeros and starts no clock.
        assert!(y[..pad].iter().all(|s| s.re == 0.0 && s.im == 0.0));
        // Reverting only the post-padding samples restores them exactly.
        let back = revert_dc(&y[pad..], FS);
        for (a, b) in tail.iter().zip(&back) {
            assert!((a - b).norm() <= 1e-6 * a.norm().max(1e-2), "{a} vs {b}");
        }
        // The running-mean start settles within 50 ms of the first sample.
        let mut b = DcBlocker::new(FS);
        for &s in &x[..pad + (FS as usize / 20)] {
            b.process_sample(s);
        }
        let l = b.level();
        assert!(
            (l.re - 0.01).abs() < 3e-3 && (l.im - 0.007).abs() < 3e-3,
            "{l}"
        );
    }

    #[test]
    fn dc_blocker_removes_a_constant_offset() {
        let mut x = noise(FS as usize, 3, 0.05);
        for s in x.iter_mut() {
            *s += Complex32::new(0.01, 0.007);
        }
        let mut b = DcBlocker::new(FS);
        for &s in &x {
            b.process_sample(s);
        }
        let l = b.level();
        assert!(
            (l.re - 0.01).abs() < 2e-3 && (l.im - 0.007).abs() < 2e-3,
            "{l}"
        );
    }

    #[test]
    fn dc_update_clamp_bounds_an_impulse_kick() {
        let x = noise(FS as usize + 1, 11, 0.05);
        let mut b = DcBlocker::new(FS);
        for &s in &x[..FS as usize] {
            b.process_sample(s);
        }
        let before = b.level();
        b.process_sample(Complex32::new(1e8, 0.0));
        let kick = (b.level() - before).norm();
        let a = std::f64::consts::TAU * DC_CORNER_HZ / FS;
        assert!(kick <= a * (1.0 + 1e-6), "kick {kick} > a {a}");
    }

    #[test]
    fn dc_corner_attenuates_60_hz_by_under_0_05_db() {
        let n = 4 * FS as usize;
        let x: Vec<Complex32> = (0..n)
            .map(|i| {
                let ph = std::f64::consts::TAU * 60.0 * i as f64 / FS;
                Complex32::new(ph.cos() as f32, ph.sin() as f32) * 0.1
            })
            .collect();
        let y = apply(&x);
        // Measure after the start-up transient.
        let tail = n / 2;
        let p_in: f64 = x[tail..].iter().map(|c| c.norm_sqr() as f64).sum();
        let p_out: f64 = y[tail..].iter().map(|c| c.norm_sqr() as f64).sum();
        let db = 10.0 * (p_out / p_in).log10();
        assert!(db.abs() < 0.05, "60 Hz attenuated by {db} dB");
    }
}
