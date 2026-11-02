//! MAN-323 D6: a streaming USB demodulator from one IQ stream to the 12 kHz
//! real audio an FT8/FT4 decoder reads.
//!
//! The window's audio span `[dial + 300, dial + 3000]` Hz is mixed down so
//! its centre (dial + 1650 Hz) lands at 0 Hz. Stage A, a Kaiser low-pass
//! (70 dB; pass 1950 Hz, stop 22 050 Hz at the input rate), decimates to
//! 24 kS/s. Stage B (70 dB; pass 1350 Hz, stop 1950 Hz at 24 kS/s, 175 taps)
//! decimates by 2. The result is mixed up by 1650 Hz at 12 kS/s and its
//! real part, times sqrt(2), is the audio. Stage B's symmetric response is
//! what rejects the lower sideband: everything below the dial sits at or
//! beyond -1950 Hz after the first mix.
//!
//! Per-sample state is `f32` and every sum runs in a fixed order, so the
//! output does not depend on how the input is chunked. Output sample `m`
//! stands for input time `m / 12000 - group_delay_s()`.

use num_complex::Complex32;
use std::f64::consts::PI;

use super::{AUDIO_RATE_HZ, RATE_STEP_HZ};

/// Stop-band attenuation of both stages, dB.
const ATTEN_DB: f64 = 70.0;
/// Half-width of the audio span around its centre, Hz.
const HALF_SPAN_HZ: f64 = 1350.0;
/// The span's centre above the dial, Hz.
const SPAN_CENTRE_HZ: f64 = 1650.0;
/// The lowest lower-sideband frequency after the first mix is this far below 0 Hz.
const LSB_EDGE_HZ: f64 = 1950.0;
/// Stage A's stop edge: anything here folds into +/-1950 Hz at 24 kS/s.
const STAGE_A_STOP_HZ: f64 = RATE_STEP_HZ as f64 - LSB_EDGE_HZ;

/// A Kaiser-window low-pass with `atten_db` stop-band attenuation, the given
/// pass and stop edges at rate `fs`, unity DC gain. Taps are designed in
/// `f64` and stored as `f32`; the length is odd so the delay is whole.
pub(crate) fn kaiser_lowpass(fs: f64, pass_hz: f64, stop_hz: f64, atten_db: f64) -> Vec<f32> {
    let dw = 2.0 * PI * (stop_hz - pass_hz) / fs;
    let mut n = ((atten_db - 7.95) / (2.285 * dw)).ceil() as usize + 1;
    if n.is_multiple_of(2) {
        n += 1;
    }
    let beta = if atten_db > 50.0 {
        0.1102 * (atten_db - 8.7)
    } else if atten_db >= 21.0 {
        0.5842 * (atten_db - 21.0).powf(0.4) + 0.07886 * (atten_db - 21.0)
    } else {
        0.0
    };
    let fc = (pass_hz + stop_hz) / 2.0 / fs; // cycles per sample
    let m = (n - 1) as f64 / 2.0;
    let i0_beta = bessel_i0(beta);
    let mut taps: Vec<f64> = (0..n)
        .map(|i| {
            let x = i as f64 - m;
            let sinc = if x == 0.0 {
                2.0 * fc
            } else {
                (2.0 * PI * fc * x).sin() / (PI * x)
            };
            let r = x / m;
            let win = bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / i0_beta;
            sinc * win
        })
        .collect();
    let sum: f64 = taps.iter().sum();
    for t in &mut taps {
        *t /= sum;
    }
    taps.into_iter().map(|t| t as f32).collect()
}

/// The zeroth-order modified Bessel function of the first kind (series).
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let half = x / 2.0;
    for k in 1..64 {
        term *= half / k as f64;
        let t2 = term * term;
        sum += t2;
        if t2 < sum * 1e-17 {
            break;
        }
    }
    sum
}

/// A decimating FIR over complex samples with real taps. Output `j` is
/// computed when input `j * factor` arrives, so it stands for input time
/// `j * factor - (taps - 1) / 2`.
#[derive(Clone, Debug)]
struct DecFir {
    taps: Vec<f32>,
    factor: usize,
    /// Twice the tap count: each sample is written at `pos` and
    /// `pos + taps.len()`, so the newest `taps.len()` samples are always
    /// contiguous.
    hist: Vec<Complex32>,
    pos: usize,
    phase: usize,
}

impl DecFir {
    fn new(taps: Vec<f32>, factor: usize) -> Self {
        let n = taps.len();
        DecFir {
            taps,
            factor,
            hist: vec![Complex32::new(0.0, 0.0); 2 * n],
            pos: 0,
            phase: 0,
        }
    }

    fn reset(&mut self) {
        self.hist.fill(Complex32::new(0.0, 0.0));
        self.pos = 0;
        self.phase = 0;
    }

    #[inline]
    fn push(&mut self, x: Complex32) -> Option<Complex32> {
        let n = self.taps.len();
        self.hist[self.pos] = x;
        self.hist[self.pos + n] = x;
        // Newest sample at hist[pos + n], oldest at hist[pos + 1].
        let window_start = self.pos + 1;
        self.pos = (self.pos + 1) % n;
        let emit = self.phase == 0;
        self.phase = (self.phase + 1) % self.factor;
        if !emit {
            return None;
        }
        let window = &self.hist[window_start..window_start + n];
        // taps[k] multiplies the sample k steps old; the filter is
        // symmetric, so taps and the oldest-first window line up directly.
        let mut acc = Complex32::new(0.0, 0.0);
        for (t, s) in self.taps.iter().zip(window) {
            acc += s * *t;
        }
        Some(acc)
    }
}

/// A numerically controlled oscillator with an `f64` phase in cycles.
#[derive(Clone, Debug)]
struct Nco {
    step: f64,
    phase: f64,
}

impl Nco {
    fn new(freq_hz: f64, fs: f64) -> Self {
        Nco {
            step: freq_hz / fs,
            phase: 0.0,
        }
    }

    #[inline]
    fn next(&mut self) -> Complex32 {
        let (s, c) = (2.0 * PI * self.phase).sin_cos();
        self.phase += self.step;
        self.phase -= self.phase.floor();
        Complex32::new(c as f32, s as f32)
    }
}

/// One window's demodulator: IQ at the stream rate in, 12 kHz audio out.
#[derive(Clone, Debug)]
pub struct UsbDemod {
    fs: f64,
    down: Nco,
    stage_a: Option<DecFir>,
    stage_b: DecFir,
    up: Nco,
    initial_down: Nco,
    initial_up: Nco,
}

impl UsbDemod {
    /// A demodulator for a stream of `fs` S/s whose window dial lies
    /// `dial_offset_hz` above the stream's centre. `None` unless `fs` is a
    /// multiple of 24 000 S/s.
    pub fn new(fs: f64, dial_offset_hz: f64) -> Option<Self> {
        if !super::rate_supported(fs) {
            return None;
        }
        let rate_step = f64::from(RATE_STEP_HZ);
        let factor_a = (fs / rate_step) as usize;
        let stage_a = (factor_a > 1).then(|| {
            DecFir::new(
                kaiser_lowpass(fs, LSB_EDGE_HZ, STAGE_A_STOP_HZ, ATTEN_DB),
                factor_a,
            )
        });
        let stage_b = DecFir::new(
            kaiser_lowpass(rate_step, HALF_SPAN_HZ, LSB_EDGE_HZ, ATTEN_DB),
            2,
        );
        let down = Nco::new(-(dial_offset_hz + SPAN_CENTRE_HZ), fs);
        let up = Nco::new(SPAN_CENTRE_HZ, f64::from(AUDIO_RATE_HZ));
        Some(UsbDemod {
            fs,
            initial_down: down.clone(),
            initial_up: up.clone(),
            down,
            stage_a,
            stage_b,
            up,
        })
    }

    /// Demodulate `iq`, appending the audio it completes to `out`.
    pub fn process(&mut self, iq: &[Complex32], out: &mut Vec<f32>) {
        out.reserve(iq.len() * AUDIO_RATE_HZ as usize / self.fs as usize + 1);
        for &x in iq {
            let mixed = x * self.down.next();
            let a = match &mut self.stage_a {
                Some(fir) => match fir.push(mixed) {
                    Some(y) => y,
                    None => continue,
                },
                None => mixed,
            };
            if let Some(b) = self.stage_b.push(a) {
                let y = b * self.up.next();
                out.push(y.re * std::f32::consts::SQRT_2);
            }
        }
    }

    /// Forget all history (after a stream discontinuity): the next output
    /// equals a fresh demodulator's.
    pub fn reset(&mut self) {
        self.down = self.initial_down.clone();
        self.up = self.initial_up.clone();
        if let Some(fir) = &mut self.stage_a {
            fir.reset();
        }
        self.stage_b.reset();
    }

    /// The two stages' combined group delay, seconds.
    pub fn group_delay_s(&self) -> f64 {
        let a = self
            .stage_a
            .as_ref()
            .map_or(0.0, |f| (f.taps.len() - 1) as f64 / 2.0 / self.fs);
        a + (self.stage_b.taps.len() - 1) as f64 / 2.0 / f64::from(RATE_STEP_HZ)
    }

    /// Tap counts of stage A (0 when the stream is already 24 kS/s) and B.
    pub fn tap_counts(&self) -> (usize, usize) {
        (
            self.stage_a.as_ref().map_or(0, |f| f.taps.len()),
            self.stage_b.taps.len(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIAL_OFFSET: f64 = 14_074_000.0 - 14_050_000.0;

    fn tone(fs: f64, freq_hz: f64, n: usize) -> Vec<Complex32> {
        (0..n)
            .map(|i| {
                let ph = 2.0 * PI * freq_hz * i as f64 / fs;
                Complex32::new(ph.cos() as f32, ph.sin() as f32)
            })
            .collect()
    }

    fn demod_all(fs: f64, iq: &[Complex32]) -> Vec<f32> {
        let mut d = UsbDemod::new(fs, DIAL_OFFSET).unwrap();
        let mut out = Vec::new();
        d.process(iq, &mut out);
        out
    }

    /// Mean square after the filters have settled (skip the first 0.1 s).
    fn settled_power(audio: &[f32]) -> f64 {
        let tail = &audio[1200..];
        tail.iter()
            .map(|&x| f64::from(x) * f64::from(x))
            .sum::<f64>()
            / tail.len() as f64
    }

    fn bin_power(x: &[f32], freq_hz: f64) -> f64 {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, &s) in x.iter().enumerate() {
            let ph = 2.0 * PI * freq_hz * i as f64 / f64::from(AUDIO_RATE_HZ);
            re += f64::from(s) * ph.cos();
            im -= f64::from(s) * ph.sin();
        }
        re * re + im * im
    }

    #[test]
    fn upper_sideband_tone_appears_at_its_audio_offset() {
        let fs = 96_000.0;
        let audio = demod_all(fs, &tone(fs, DIAL_OFFSET + 1000.0, 96_000));
        let seg = &audio[1200..1200 + 4096];
        let bin_hz = f64::from(AUDIO_RATE_HZ) / seg.len() as f64;
        let peak = (1..seg.len() / 2)
            .map(|k| (k, bin_power(seg, k as f64 * bin_hz)))
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .unwrap()
            .0;
        let peak_hz = peak as f64 * bin_hz;
        assert!((peak_hz - 1000.0).abs() <= bin_hz, "peak at {peak_hz} Hz");
    }

    #[test]
    fn lower_sideband_is_rejected_by_60_db() {
        let fs = 96_000.0;
        for x in [300.0, 1000.0, 2500.0] {
            let usb = settled_power(&demod_all(fs, &tone(fs, DIAL_OFFSET + x, 48_000)));
            let lsb = settled_power(&demod_all(fs, &tone(fs, DIAL_OFFSET - x, 48_000)));
            let rejection_db = 10.0 * (usb / lsb).log10();
            assert!(rejection_db >= 60.0, "x = {x} Hz: {rejection_db:.1} dB");
        }
    }

    #[test]
    fn passband_is_flat_within_1_db_from_300_to_3000_hz() {
        let fs = 48_000.0;
        for audio_hz in (300..=3000).step_by(150) {
            let p = settled_power(&demod_all(
                fs,
                &tone(fs, DIAL_OFFSET + f64::from(audio_hz), 24_000),
            ));
            let db = 10.0 * p.log10();
            assert!(db.abs() <= 1.0, "{audio_hz} Hz: {db:.2} dB");
        }
    }

    #[test]
    fn output_does_not_depend_on_chunk_size() {
        let fs = 96_000.0;
        // A deterministic pseudo-random 2 s buffer (an LCG; no RNG crate here).
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            ((state >> 40) as f32 / (1u64 << 24) as f32) - 0.5
        };
        let iq: Vec<Complex32> = (0..192_000)
            .map(|_| Complex32::new(next(), next()))
            .collect();
        let whole = demod_all(fs, &iq);
        for chunk in [2048, 1000] {
            let mut d = UsbDemod::new(fs, DIAL_OFFSET).unwrap();
            let mut out = Vec::new();
            for c in iq.chunks(chunk) {
                d.process(c, &mut out);
            }
            assert_eq!(out.len(), whole.len(), "chunk {chunk}");
            assert!(
                out.iter()
                    .zip(&whole)
                    .all(|(a, b)| a.to_bits() == b.to_bits()),
                "chunk {chunk} output differs"
            );
        }
    }

    #[test]
    fn group_delay_matches_the_filter_lengths() {
        for (fs, taps_a) in [
            (48_000.0, 13),
            (96_000.0, 23),
            (192_000.0, 43),
            (384_000.0, 85),
        ] {
            let d = UsbDemod::new(fs, DIAL_OFFSET).unwrap();
            assert_eq!(d.tap_counts(), (taps_a, 175), "{fs}");
            let want = (taps_a - 1) as f64 / 2.0 / fs + 174.0 / 2.0 / 24_000.0;
            assert!((d.group_delay_s() - want).abs() < 1e-12, "{fs}");
            assert!((d.group_delay_s() - 0.00374).abs() < 0.0001, "{fs}");
        }
        assert!(UsbDemod::new(250_000.0, DIAL_OFFSET).is_none());
        assert!(UsbDemod::new(44_100.0, DIAL_OFFSET).is_none());
    }

    #[test]
    fn reset_clears_history() {
        let fs = 96_000.0;
        let first = tone(fs, DIAL_OFFSET + 700.0, 30_011);
        let second = tone(fs, DIAL_OFFSET + 1900.0, 20_000);
        let mut d = UsbDemod::new(fs, DIAL_OFFSET).unwrap();
        let mut scratch = Vec::new();
        d.process(&first, &mut scratch);
        d.reset();
        let mut after_reset = Vec::new();
        d.process(&second, &mut after_reset);
        let fresh = demod_all(fs, &second);
        assert_eq!(after_reset.len(), fresh.len());
        assert!(after_reset
            .iter()
            .zip(&fresh)
            .all(|(a, b)| a.to_bits() == b.to_bits()));
    }

    #[test]
    fn output_rate_is_12_khz() {
        for fs in [48_000.0, 96_000.0, 192_000.0] {
            let n = fs as usize; // one second
            assert_eq!(
                demod_all(fs, &tone(fs, DIAL_OFFSET + 1000.0, n)).len(),
                12_000
            );
        }
    }
}
