//! Experimental per-track coherent two-channel reconstruction.
//! Uses the unchanged PFB hop rate and keeps NCO/filter state across retunes.
use crate::channelizer::{
    odd_channel_sign_correction, CHANNELS_PER_PHASE_CYCLE, CHANNEL_SPACING_HZ,
};
use num_complex::Complex32;

const TAPS: usize = 31;
/// Symmetric FIR latency in channelizer hops.
pub const GROUP_DELAY_HOPS: usize = (TAPS - 1) / 2;
const HOP_RATE_HZ: f64 = CHANNEL_SPACING_HZ * CHANNELS_PER_PHASE_CYCLE;

/// Decoder-only frontend. Detection and raw noise observations stay broadband.
pub struct SteeredFrontend {
    taps: [f32; TAPS],
    history: [Complex32; TAPS],
    phase: f64,
    last_pos: Option<f64>,
}
impl Default for SteeredFrontend {
    fn default() -> Self {
        let mut taps = [0.0; TAPS];
        let mut sum = 0.0;
        for (i, tap) in taps.iter_mut().enumerate() {
            let x = i as f64 - GROUP_DELAY_HOPS as f64;
            let a = std::f64::consts::TAU * 30.0 / HOP_RATE_HZ * x;
            let sinc = if a == 0.0 { 1.0 } else { a.sin() / a };
            let window = 0.42 - 0.5 * (std::f64::consts::TAU * i as f64 / (TAPS - 1) as f64).cos()
                + 0.08 * (2.0 * std::f64::consts::TAU * i as f64 / (TAPS - 1) as f64).cos();
            *tap = (sinc * window) as f32;
            sum += *tap;
        }
        for t in &mut taps {
            *t /= sum;
        }
        Self {
            taps,
            history: [Complex32::new(0.0, 0.0); TAPS],
            phase: 0.0,
            last_pos: None,
        }
    }
}
impl SteeredFrontend {
    /// Absolute-frame reconstruction avoids a phase discontinuity when the
    /// rounded home changes. Both channels are phase-aligned at hop `m`.
    pub fn push(
        &mut self,
        x_home: Complex32,
        x_partner: Complex32,
        home: usize,
        partner: usize,
        pos: f64,
        m: u64,
    ) -> f32 {
        let rotate = |x: Complex32, k: usize| {
            x * odd_channel_sign_correction(m, k)
                * Complex32::from_polar(
                    1.0,
                    (std::f64::consts::TAU * (k % 4) as f64 * (m % 4) as f64
                        / CHANNELS_PER_PHASE_CYCLE) as f32,
                )
        };
        if let Some(previous) = self.last_pos {
            self.phase = (self.phase + std::f64::consts::TAU * previous / CHANNELS_PER_PHASE_CYCLE)
                .rem_euclid(std::f64::consts::TAU);
        } else {
            self.phase = (std::f64::consts::TAU * pos * m as f64 / CHANNELS_PER_PHASE_CYCLE)
                .rem_euclid(std::f64::consts::TAU);
        }
        self.last_pos = Some(pos);
        let sample = (rotate(x_home, home) + rotate(x_partner, partner))
            * Complex32::from_polar(1.0, -self.phase as f32);
        self.history.rotate_left(1);
        self.history[TAPS - 1] = sample;
        let mut value = num_complex::Complex64::new(0.0, 0.0);
        for (&x, &tap) in self.history.iter().zip(self.taps.iter().rev()) {
            value += num_complex::Complex64::new(x.re as f64, x.im as f64) * tap as f64;
        }
        value.norm() as f32
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::channelizer::Channelizer;
    fn tone_power(offset: f64, steer: f64) -> f64 {
        let fs = 48000.0;
        let samples: Vec<_> = (0..48000)
            .map(|i| {
                Complex32::from_polar(
                    1.0,
                    (std::f64::consts::TAU * (10.0 + offset) * CHANNEL_SPACING_HZ * i as f64 / fs)
                        as f32,
                )
            })
            .collect();
        let mut channelizer = Channelizer::new(fs, 0.0).unwrap();
        let mut frontend = SteeredFrontend::default();
        let home = (10.0 + steer).round() as usize;
        let partner = if 10.0 + steer >= home as f64 {
            home + 1
        } else {
            home - 1
        };
        let mut values = Vec::new();
        for h in channelizer.process(&samples) {
            let a = frontend.push(h.x[home], h.x[partner], home, partner, 10.0 + steer, h.m);
            if h.m > 100 {
                values.push((a * a) as f64);
            }
        }
        values.iter().sum::<f64>() / values.len() as f64
    }
    #[test]
    fn steered_output_is_flat_across_the_channel() {
        let center = tone_power(0.0, 0.0);
        for offset in [0.0, 0.125, 0.25, 0.375, 0.5] {
            let db = 10.0 * (tone_power(offset, offset) / center).log10();
            assert!(db.abs() < 1.0, "offset {offset}: {db} dB");
        }
    }
    #[test]
    fn steered_output_rejects_a_tone_100_hz_away_by_at_least_40_db() {
        let db =
            10.0 * (tone_power(100.0 / CHANNEL_SPACING_HZ, 0.0) / tone_power(0.0, 0.0)).log10();
        assert!(db <= -40.0, "rejection {db} dB");
    }
    #[test]
    fn retune_within_a_channel_does_not_reset_the_filter() {
        let mut f = SteeredFrontend::default();
        for m in 0..100 {
            let pos = 10.0 + m as f64 * 0.0001;
            // Supply a sample in phase with the frontend's continuous NCO.
            let phase = if let Some(previous) = f.last_pos {
                f.phase + std::f64::consts::TAU * previous / CHANNELS_PER_PHASE_CYCLE
            } else {
                0.0
            };
            let absolute = Complex32::from_polar(1.0, phase as f32);
            let x = absolute
                * Complex32::from_polar(
                    1.0,
                    (-std::f64::consts::FRAC_PI_2 * 10.0 * m as f64) as f32,
                );
            let amplitude = f.push(x, Complex32::new(0.0, 0.0), 10, 11, pos, m);
            if m > 40 {
                assert!((amplitude - 1.0).abs() < 0.001);
            }
        }
    }
    #[test]
    fn retune_across_a_channel_preserves_the_absolute_frame() {
        let mut a = SteeredFrontend::default();
        let mut b = SteeredFrontend::default();
        for m in 0..100 {
            let pos = 10.4 + m as f64 * 0.002;
            let phase = if let Some(previous) = a.last_pos {
                a.phase + std::f64::consts::TAU * previous / CHANNELS_PER_PHASE_CYCLE
            } else {
                0.0
            };
            let x = |k: usize| {
                Complex32::from_polar(
                    0.5,
                    (phase - std::f64::consts::FRAC_PI_2 * k as f64 * m as f64) as f32,
                ) * odd_channel_sign_correction(m, k)
            };
            let aa = a.push(x(10), x(11), 10, 11, pos, m);
            let (home, partner) = if m < 50 { (10, 11) } else { (11, 10) };
            let bb = b.push(x(home), x(partner), home, partner, pos, m);
            assert!((aa - bb).abs() < 1e-5, "retune hop {m}");
        }
    }
    #[test]
    fn steered_group_delay_is_a_whole_number_of_hops() {
        assert_eq!(2 * GROUP_DELAY_HOPS + 1, TAPS);
    }
}
