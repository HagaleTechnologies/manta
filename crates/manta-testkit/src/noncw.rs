//! MAN-313: deterministic non-CW signal generators, so the track manager's
//! non-CW qualification can be tested against the scenes the ticket names
//! (SSB voice, wideband data, swept carriers, a beacon-style carrier).
//!
//! Every generator *adds* into an existing analytic IQ buffer (as
//! `fsk::add_fsk` does) that `scene::render_scene` has already scaled by
//! `MASTER_SCALE`, so each level is `amplitude_for_snr_2500(snr, fs) *
//! MASTER_SCALE`, the key-down level of a CW carrier at the same SNR.
//! Randomness is ChaCha8 seeded per call and consumed only through the
//! crate's `u01`/`gaussian_pair`, so output is bit-stable per seed.
//!
//! Models and their limits:
//!
//! - [`add_ssb_voice`]: upper-sideband voice as a sum of harmonics of a
//!   fundamental (100-150 Hz per phrase, 20 % declination across the
//!   phrase, 3 Hz vibrato at 5 Hz), each harmonic weighted by one of six
//!   vowel formant sets (three Lorentzian formants, 80/100/120 Hz wide,
//!   gains 1/0.6/0.3), a `(f/300)^-0.7` tilt and raised-cosine tapers at
//!   250-350 Hz and 2700-2900 Hz above the suppressed carrier. Syllables
//!   are 120-300 ms (20 ms attack, 50 ms decay) with 10-60 ms gaps, in
//!   phrases of 1.5-4 s separated by 0.3-1 s pauses. Limits: no
//!   consonants, noise or formant glides; a syllable is a steady plateau
//!   with smooth edges, which is more CW-like (on/off keyed lines) than
//!   real speech, so this is a conservative model for a CW filter.
//! - [`add_band_noise`]: wideband data (OFDM/PSK-like) as complex Gaussian
//!   noise through four cascaded boxcars, length
//!   `round(0.31 * fs / (bw/2))`, which puts the band edges about 5.5 dB
//!   down and the first sidelobe about 53 dB down. Limits: no carriers,
//!   pilots or symbol framing; stationary, so it never keys.
//! - [`add_sweep`]: a phase-continuous linear chirp (a tuning carrier or
//!   a sweeping interferer) from `f0` to `f1`, off outside its span, with
//!   an optional raised-cosine ramp at each end. Limits: constant
//!   amplitude and rate; a `ramp_ms` of 0 switches on with a click whose
//!   spectrum is wideband by construction.
//! - [`add_cw_then_carrier`]: a beacon/tune-up pattern, `text` keyed once
//!   by the testkit keyer (5 ms edges), a 7-dit gap, then a steady carrier
//!   with a 5 ms rise. Limits: no QSB or drift; one keying pass.

use crate::keyer::{key_text, KeyerSpec};
use crate::noise::amplitude_for_snr_2500;
use crate::scene::MASTER_SCALE;
use crate::{gaussian_pair, u01};
use num_complex::{Complex32, Complex64};
use rand_chacha::ChaCha8Rng;
use rand_core::{Rng, SeedableRng};
use std::f64::consts::{PI, TAU};

/// (F1, F2, F3) in Hz for six vowels: /a/, /e/, /i/, /o/, /u/, /ae/.
const VOWELS: [[f64; 3]; 6] = [
    [730.0, 1090.0, 2440.0],
    [530.0, 1840.0, 2480.0],
    [270.0, 2290.0, 3010.0],
    [570.0, 840.0, 2410.0],
    [300.0, 870.0, 2240.0],
    [660.0, 1720.0, 2410.0],
];
const FORMANT_BW_HZ: [f64; 3] = [80.0, 100.0, 120.0];
const FORMANT_GAIN: [f64; 3] = [1.0, 0.6, 0.3];
/// Lower occupied edge above the carrier: zero below, taper to 350 Hz.
const VOICE_LO_HZ: f64 = 250.0;
const VOICE_LO_FLAT_HZ: f64 = 350.0;
/// Upper taper from 2700 Hz, zero above 2900 Hz.
const VOICE_HI_FLAT_HZ: f64 = 2700.0;
const VOICE_HI_HZ: f64 = 2900.0;
/// Enough harmonics for the lowest fundamental (about 77 Hz) to reach
/// `VOICE_HI_HZ`.
const MAX_HARMONICS: usize = 40;

/// Key-down amplitude of a CW carrier at `snr_2500_db`, in buffer units.
fn level(snr_2500_db: f32, fs: f64) -> f64 {
    (amplitude_for_snr_2500(snr_2500_db, fs) * MASTER_SCALE) as f64
}

fn uniform(rng: &mut ChaCha8Rng, lo: f64, hi: f64) -> f64 {
    lo + (hi - lo) * u01(rng)
}

/// Raised-cosine rise from 0 at `t = 0` to 1 at `t = len`.
fn rise(t: f64, len: f64) -> f64 {
    if len <= 0.0 || t >= len {
        1.0
    } else if t <= 0.0 {
        0.0
    } else {
        0.5 * (1.0 - (PI * t / len).cos())
    }
}

/// `e^{j 2 pi f i / fs}` without phase-accumulator drift.
fn nco(f: f64, i: usize, fs: f64) -> Complex64 {
    let (s, c) = (TAU * (f * i as f64 / fs).fract()).sin_cos();
    Complex64::new(c, s)
}

fn voice_taper(f: f64) -> f64 {
    if f <= VOICE_LO_HZ || f >= VOICE_HI_HZ {
        0.0
    } else if f < VOICE_LO_FLAT_HZ {
        rise(f - VOICE_LO_HZ, VOICE_LO_FLAT_HZ - VOICE_LO_HZ)
    } else if f > VOICE_HI_FLAT_HZ {
        rise(VOICE_HI_HZ - f, VOICE_HI_HZ - VOICE_HI_FLAT_HZ)
    } else {
        1.0
    }
}

/// Amplitude of a harmonic at `f` Hz above the carrier for `vowel`.
fn harmonic_amplitude(vowel: usize, f: f64) -> f64 {
    let taper = voice_taper(f);
    if taper == 0.0 {
        return 0.0;
    }
    let formants: f64 = VOWELS[vowel]
        .iter()
        .zip(FORMANT_BW_HZ)
        .zip(FORMANT_GAIN)
        .map(|((&fk, bw), g)| {
            let x = (f - fk) / (bw / 2.0);
            g / (1.0 + x * x)
        })
        .sum();
    formants * (f / 300.0).powf(-0.7) * taper
}

/// Adds harmonic USB voice at `offset_hz` (the suppressed carrier; the
/// voice occupies `offset_hz + 250 .. offset_hz + 2900` Hz), silent before
/// `start_s`. The RMS over samples where the syllable envelope is above
/// zero is `amplitude_for_snr_2500(snr_2500_db, fs) * MASTER_SCALE`. See
/// the module doc for the model.
pub fn add_ssb_voice(
    iq: &mut [Complex32],
    fs: f64,
    offset_hz: f64,
    snr_2500_db: f32,
    seed: u64,
    start_s: f64,
) {
    let n = iq.len();
    let n_s = n as f64 / fs;
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut voice = vec![Complex32::new(0.0, 0.0); n];
    let mut active_power = 0.0f64;
    let mut active = 0usize;
    // Harmonic amplitudes are refreshed once per ~1 ms block.
    let block = ((fs * 1e-3).round() as usize).max(1);
    let mut amps = [0.0f64; MAX_HARMONICS + 1];
    // Fundamental phase; harmonic h sits at h times it, so every harmonic
    // stays phase-continuous while the fundamental moves.
    let mut phi0 = 0.0f64;
    let mut t = start_s.max(0.0);
    while t < n_s {
        let phrase_start = t;
        let phrase_len = uniform(&mut rng, 1.5, 4.0);
        let f0_base = uniform(&mut rng, 100.0, 150.0);
        let mut s = phrase_start;
        let mut phrase_end = phrase_start;
        while s - phrase_start < phrase_len && s < n_s {
            let syl_len = uniform(&mut rng, 0.120, 0.300);
            let vowel = (rng.next_u64() % VOWELS.len() as u64) as usize;
            let i0 = (s * fs).round() as usize;
            let i1 = (((s + syl_len) * fs).round() as usize).min(n);
            let mut i = i0;
            while i < i1 {
                let b_end = (i + block).min(i1);
                let tm = (i + b_end) as f64 / 2.0 / fs;
                let decl = ((tm - phrase_start) / phrase_len).clamp(0.0, 1.0);
                let f0 = f0_base * (1.0 - 0.2 * decl) + 3.0 * (TAU * 5.0 * tm).sin();
                let top = ((VOICE_HI_HZ / f0) as usize).min(MAX_HARMONICS);
                for (h, a) in amps.iter_mut().enumerate().skip(1) {
                    *a = if h <= top {
                        harmonic_amplitude(vowel, h as f64 * f0)
                    } else {
                        0.0
                    };
                }
                let dphi0 = TAU * f0 / fs;
                for (k, out) in voice[i..b_end].iter_mut().enumerate() {
                    let idx = i + k;
                    let ts = (idx - i0) as f64 / fs;
                    let env = rise(ts, 0.020).min(rise(syl_len - ts, 0.050));
                    phi0 += dphi0;
                    if phi0 >= TAU {
                        phi0 -= TAU;
                    }
                    if env <= 0.0 {
                        continue;
                    }
                    let (sn, cs) = phi0.sin_cos();
                    let z = Complex64::new(cs, sn);
                    // Horner: sum over h of a_h z^h.
                    let mut acc = Complex64::new(0.0, 0.0);
                    for &a in amps[1..=top].iter().rev() {
                        acc = (acc + a) * z;
                    }
                    let v = acc * env * nco(offset_hz, idx, fs);
                    active_power += v.norm_sqr();
                    active += 1;
                    *out = Complex32::new(v.re as f32, v.im as f32);
                }
                i = b_end;
            }
            phrase_end = s + syl_len;
            s += syl_len + uniform(&mut rng, 0.010, 0.060);
        }
        t = phrase_end + uniform(&mut rng, 0.3, 1.0);
    }
    if active == 0 || active_power <= 0.0 {
        return;
    }
    let scale = (level(snr_2500_db, fs) / (active_power / active as f64).sqrt()) as f32;
    for (out, v) in iq.iter_mut().zip(voice.iter()) {
        *out += v * scale;
    }
}

/// One moving-average stage of length `ring.len()`.
struct Boxcar {
    ring: Vec<Complex64>,
    pos: usize,
    sum: Complex64,
}

impl Boxcar {
    fn new(len: usize) -> Self {
        Boxcar {
            ring: vec![Complex64::new(0.0, 0.0); len],
            pos: 0,
            sum: Complex64::new(0.0, 0.0),
        }
    }

    fn push(&mut self, x: Complex64) -> Complex64 {
        self.sum += x - self.ring[self.pos];
        self.ring[self.pos] = x;
        self.pos = (self.pos + 1) % self.ring.len();
        self.sum / self.ring.len() as f64
    }
}

/// Adds stationary band-limited noise `bw_hz` wide (about -5.5 dB at the
/// edges) centred `center_offset_hz` from the IQ centre, with RMS
/// `amplitude_for_snr_2500(snr_2500_db, fs) * MASTER_SCALE` over the
/// buffer. See the module doc for the model.
pub fn add_band_noise(
    iq: &mut [Complex32],
    fs: f64,
    center_offset_hz: f64,
    bw_hz: f64,
    snr_2500_db: f32,
    seed: u64,
) {
    let n = iq.len();
    if n == 0 {
        return;
    }
    let len = ((0.31 * fs / (bw_hz / 2.0)).round() as usize).max(1);
    let mut stages: Vec<Boxcar> = (0..4).map(|_| Boxcar::new(len)).collect();
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    // Run the cascade through its transient so sample 0 is stationary.
    let warm = 4 * len;
    let mut noise = vec![Complex64::new(0.0, 0.0); n];
    let mut power = 0.0f64;
    for i in 0..warm + n {
        let (zi, zq) = gaussian_pair(&mut rng);
        let mut x = Complex64::new(zi, zq);
        for st in &mut stages {
            x = st.push(x);
        }
        if i >= warm {
            power += x.norm_sqr();
            noise[i - warm] = x;
        }
    }
    if power <= 0.0 {
        return;
    }
    let scale = level(snr_2500_db, fs) / (power / n as f64).sqrt();
    for (i, (out, x)) in iq.iter_mut().zip(noise.iter()).enumerate() {
        let v = x * scale * nco(center_offset_hz, i, fs);
        out.re += v.re as f32;
        out.im += v.im as f32;
    }
}

/// Adds a linear chirp that starts at `start_s` at `f0_offset_hz` and
/// moves at `|rate_hz_per_s|` towards `f1_offset_hz`, switching off when
/// it reaches `f1_offset_hz` or the buffer ends. `ramp_ms > 0` shapes a
/// raised-cosine rise at the start and fall at `f1`; 0 is abrupt. Level
/// is a CW carrier's at `snr_2500_db`.
#[allow(clippy::too_many_arguments)]
pub fn add_sweep(
    iq: &mut [Complex32],
    fs: f64,
    f0_offset_hz: f64,
    f1_offset_hz: f64,
    rate_hz_per_s: f64,
    snr_2500_db: f32,
    start_s: f64,
    ramp_ms: f64,
) {
    let span = f1_offset_hz - f0_offset_hz;
    let rate = rate_hz_per_s.abs();
    if rate == 0.0 || span == 0.0 {
        return;
    }
    let k = rate * span.signum();
    let sweep_s = span.abs() / rate;
    let amp = level(snr_2500_db, fs);
    let ramp_s = ramp_ms.max(0.0) / 1000.0;
    let i0 = (start_s.max(0.0) * fs).round() as usize;
    let i1 = (((start_s + sweep_s) * fs).round() as usize).min(iq.len());
    for (i, out) in iq.iter_mut().enumerate().take(i1).skip(i0) {
        let tau = i as f64 / fs - start_s;
        // Closed-form phase: f0*tau + k*tau^2/2 cycles, no accumulator.
        let cycles = f0_offset_hz * tau + 0.5 * k * tau * tau;
        let env = rise(tau, ramp_s).min(rise(sweep_s - tau, ramp_s));
        let (s, c) = (TAU * cycles.fract()).sin_cos();
        out.re += (amp * env * c) as f32;
        out.im += (amp * env * s) as f32;
    }
}

/// Adds `text` keyed once at `wpm` (testkit keyer, 5 ms edges) from t = 0,
/// a 7-dit gap, then a steady carrier with a 5 ms raised-cosine rise until
/// `carrier_until_s` (a 5 ms fall ending there if inside the buffer), all
/// at `offset_hz` and a CW carrier's level. Returns the carrier start time
/// in seconds: keying end + 7 dits.
///
/// # Panics
/// If `text` holds a character the keyer cannot send.
pub fn add_cw_then_carrier(
    iq: &mut [Complex32],
    fs: f64,
    offset_hz: f64,
    snr_2500_db: f32,
    text: &str,
    wpm: f64,
    carrier_until_s: f64,
) -> f64 {
    let (env, _) = key_text(text, &KeyerSpec::new(wpm as f32), fs)
        .expect("add_cw_then_carrier: text must be keyable Morse");
    let dit_s = 1.2 / wpm;
    let carrier_start_s = env.len() as f64 / fs + 7.0 * dit_s;
    let amp = level(snr_2500_db, fs);
    let ramp_s = 0.005;
    let c0 = (carrier_start_s * fs).round() as usize;
    let c1 = ((carrier_until_s * fs).round() as usize).min(iq.len());
    let fall = c1 < iq.len();
    for (i, out) in iq.iter_mut().enumerate() {
        let e = if i < env.len() {
            env[i] as f64
        } else if i >= c0 && i < c1 {
            let t = (i - c0) as f64 / fs;
            let up = rise(t, ramp_s);
            if fall {
                up.min(rise((c1 - i) as f64 / fs, ramp_s))
            } else {
                up
            }
        } else {
            0.0
        };
        if e != 0.0 {
            let v = nco(offset_hz, i, fs) * (amp * e);
            out.re += v.re as f32;
            out.im += v.im as f32;
        }
    }
    carrier_start_s
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 96_000.0;

    fn zeros(seconds: f64, fs: f64) -> Vec<Complex32> {
        vec![Complex32::new(0.0, 0.0); (seconds * fs).round() as usize]
    }

    fn target(snr: f32, fs: f64) -> f64 {
        (amplitude_for_snr_2500(snr, fs) * MASTER_SCALE) as f64
    }

    /// In-place iterative radix-2 FFT.
    fn fft(buf: &mut [Complex64]) {
        let n = buf.len();
        assert!(n.is_power_of_two());
        let mut j = 0usize;
        for i in 1..n {
            let mut bit = n >> 1;
            while j & bit != 0 {
                j ^= bit;
                bit >>= 1;
            }
            j |= bit;
            if i < j {
                buf.swap(i, j);
            }
        }
        let mut len = 2;
        while len <= n {
            let half = len / 2;
            for chunk in buf.chunks_mut(len) {
                for k in 0..half {
                    let w = Complex64::from_polar(1.0, -TAU * k as f64 / len as f64);
                    let u = chunk[k];
                    let v = chunk[k + half] * w;
                    chunk[k] = u + v;
                    chunk[k + half] = u - v;
                }
            }
            len <<= 1;
        }
    }

    /// Hann-windowed power spectrum: (frequency Hz, power) per bin.
    fn spectrum(x: &[Complex32], fs: f64) -> Vec<(f64, f64)> {
        let n = x.len();
        let mut buf: Vec<Complex64> = x
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let w = 0.5 * (1.0 - (TAU * i as f64 / n as f64).cos());
                Complex64::new(s.re as f64, s.im as f64) * w
            })
            .collect();
        fft(&mut buf);
        buf.iter()
            .enumerate()
            .map(|(k, c)| {
                let k = if k < n / 2 {
                    k as f64
                } else {
                    k as f64 - n as f64
                };
                (k * fs / n as f64, c.norm_sqr())
            })
            .collect()
    }

    fn band_mean(spec: &[(f64, f64)], lo: f64, hi: f64) -> f64 {
        let v: Vec<f64> = spec
            .iter()
            .filter(|&&(f, _)| f >= lo && f <= hi)
            .map(|&(_, p)| p)
            .collect();
        v.iter().sum::<f64>() / v.len() as f64
    }

    #[test]
    fn ssb_voice_occupies_its_passband_only() {
        let offset = 40_000.0;
        let mut iq = zeros(2.0, FS);
        add_ssb_voice(&mut iq, FS, offset, 30.0, 1, 0.0);
        // About 1.4 s from t = 0, inside the first phrase (>= 1.5 s).
        let spec = spectrum(&iq[..1 << 17], FS);
        let total: f64 = spec.iter().map(|&(_, p)| p).sum();
        let inside: f64 = spec
            .iter()
            .filter(|&&(f, _)| f >= offset + 250.0 && f <= offset + 2_900.0)
            .map(|&(_, p)| p)
            .sum();
        assert!(total > 0.0);
        let frac = inside / total;
        assert!(frac >= 0.9, "in-band energy fraction {frac}");
    }

    #[test]
    fn ssb_voice_active_level_matches_snr() {
        let snr = 20.0;
        let mut iq = zeros(10.0, FS);
        add_ssb_voice(&mut iq, FS, 40_000.0, snr, 2, 0.5);
        let active: Vec<f64> = iq
            .iter()
            .map(|s| s.norm_sqr() as f64)
            .filter(|&p| p > 0.0)
            .collect();
        // Speech fills a good part of the buffer but not all of it.
        let duty = active.len() as f64 / iq.len() as f64;
        assert!(duty > 0.3 && duty < 0.95, "duty {duty}");
        assert!(iq[..(0.5 * FS) as usize]
            .iter()
            .all(|s| s.norm_sqr() == 0.0));
        let rms = (active.iter().sum::<f64>() / active.len() as f64).sqrt();
        let db = 20.0 * (rms / target(snr, FS)).log10();
        assert!(db.abs() <= 0.5, "active level off by {db} dB");
    }

    #[test]
    fn band_noise_is_flat_inside_and_quiet_outside() {
        let (c, bw, snr) = (10_000.0, 2_800.0, 30.0);
        let mut iq = zeros(1.5, FS);
        add_band_noise(&mut iq, FS, c, bw, snr, 3);
        let p: f64 = iq.iter().map(|s| s.norm_sqr() as f64).sum::<f64>() / iq.len() as f64;
        let db = 10.0 * (p / target(snr, FS).powi(2)).log10();
        assert!(db.abs() < 0.01, "total level off by {db} dB");

        let spec = spectrum(&iq[..1 << 17], FS);
        let db_of = |lo: f64, hi: f64, re: f64| 10.0 * (band_mean(&spec, lo, hi) / re).log10();
        let centre = band_mean(&spec, c - 100.0, c + 100.0);
        for d in [-600.0, -300.0, 300.0, 600.0] {
            let rel = db_of(c + d - 100.0, c + d + 100.0, centre);
            assert!(rel.abs() < 2.0, "{d:+} Hz: {rel} dB vs centre");
        }
        let lower = db_of(c - 10_000.0, c - 1.5 * bw, centre);
        let upper = db_of(c + 1.5 * bw, c + 10_000.0, centre);
        assert!(lower < -40.0 && upper < -40.0, "{lower} / {upper} dB");
    }

    /// Instantaneous frequency between samples `i` and `i + 1`.
    fn inst_freq(iq: &[Complex32], i: usize, fs: f64) -> f64 {
        let d = Complex64::new(iq[i + 1].re as f64, iq[i + 1].im as f64)
            * Complex64::new(iq[i].re as f64, -iq[i].im as f64);
        d.arg() * fs / TAU
    }

    #[test]
    fn sweep_moves_at_the_requested_rate() {
        let (f0, f1, rate, start) = (-20_000.0, 20_000.0, 2_000.0, 0.5);
        let mut iq = zeros(3.0, FS);
        add_sweep(&mut iq, FS, f0, f1, rate, 20.0, start, 0.0);
        for t in [1.0, 2.25] {
            let i = (t * FS) as usize;
            let tau = (i as f64 + 0.5) / FS - start;
            let want = f0 + rate * tau;
            let got = inst_freq(&iq, i, FS);
            assert!((got - want).abs() < 1.0, "t {t}: {got} vs {want}");
        }
        assert!(iq[(0.49 * FS) as usize].norm_sqr() == 0.0);

        // Downward, and off once it reaches f1 (at start + 0.5 s).
        let mut down = zeros(2.0, FS);
        add_sweep(&mut down, FS, 3_000.0, 2_000.0, 2_000.0, 20.0, 0.25, 0.0);
        let i = (0.5 * FS) as usize;
        let want = 3_000.0 - 2_000.0 * ((i as f64 + 0.5) / FS - 0.25);
        assert!((inst_freq(&down, i, FS) - want).abs() < 1.0);
        assert!(down[(0.76 * FS) as usize..]
            .iter()
            .all(|s| s.norm_sqr() == 0.0));
    }

    #[test]
    fn sweep_ramp_softens_the_switch_on() {
        let amp = target(20.0, FS);
        let start = 0.1;
        let i0 = (start * FS).round() as usize;
        let mut soft = zeros(0.5, FS);
        add_sweep(&mut soft, FS, 1_000.0, 5_000.0, 1_000.0, 20.0, start, 5.0);
        for k in 0..=(0.006 * FS) as usize {
            let t = k as f64 / FS;
            let want = if t >= 0.005 {
                1.0
            } else {
                0.5 * (1.0 - (PI * t / 0.005).cos())
            };
            let got = soft[i0 + k].norm() as f64 / amp;
            assert!((got - want).abs() < 1e-3, "{t} s: {got} vs {want}");
        }
        let mut hard = zeros(0.5, FS);
        add_sweep(&mut hard, FS, 1_000.0, 5_000.0, 1_000.0, 20.0, start, 0.0);
        assert!((hard[i0].norm() as f64 / amp - 1.0).abs() < 1e-3);
        assert!(hard[i0 - 1].norm_sqr() == 0.0);
    }

    #[test]
    fn cw_then_carrier_returns_the_keying_end_time() {
        let fs = 48_000.0;
        let amp = target(20.0, fs);
        let mut iq = zeros(8.0, fs);
        let start = add_cw_then_carrier(&mut iq, fs, 1_000.0, 20.0, "VVV TEST", 20.0, 8.0);
        // VVV (33 units) + word gap (7) + TEST (21) = 61 units of 60 ms,
        // then 7 dits.
        let want = 61.0 * 0.06 + 7.0 * 0.06;
        assert!((start - want).abs() < 1.0 / fs, "{start} vs {want}");
        let gap = ((61.0 * 0.06 + 0.01) * fs) as usize..((start - 0.01) * fs) as usize;
        assert!(iq[gap].iter().all(|s| s.norm_sqr() == 0.0));
        let steady = ((start + 0.005) * fs).ceil() as usize;
        for s in &iq[steady..] {
            assert!((s.norm() as f64 / amp - 1.0).abs() < 1e-4, "{}", s.norm());
        }
        // Keyed part reaches the same level.
        let peak = iq[..(3.0 * fs) as usize]
            .iter()
            .map(|s| s.norm() as f64)
            .fold(0.0, f64::max);
        assert!((peak / amp - 1.0).abs() < 1e-4);
    }

    #[test]
    fn generators_are_deterministic_per_seed() {
        let fs = 48_000.0;
        let render = |seed: u64| {
            let mut iq = zeros(3.0, fs);
            add_ssb_voice(&mut iq, fs, 5_000.0, 25.0, seed, 0.2);
            add_band_noise(&mut iq, fs, -8_000.0, 2_800.0, 25.0, seed);
            add_sweep(&mut iq, fs, -2_000.0, 2_000.0, 2_000.0, 20.0, 0.3, 5.0);
            add_cw_then_carrier(&mut iq, fs, 12_000.0, 20.0, "TEST", 25.0, 3.0);
            iq
        };
        let a = render(7);
        assert_eq!(a, render(7));
        assert_ne!(a, render(8));
    }
}
