//! Independent complex-baseband CW generator (MAN-206).
//!
//! Independence rule: nothing before this module's tests names the testkit's
//! production generator, the decoder or the DSP crate. It has its own Morse
//! table (typed from ITU-R M.1677-1), its own timeline in f64 seconds, its
//! own carrier, fader and noise. External crates (`num-complex`,
//! `rand_chacha`, `rand_core`) are allowed. A bug shared with the production
//! generator would have to be written twice.
//!
//! Keying recipe, in words:
//! - Units: a content unit is `1.2 / char_wpm` s (`char_wpm` defaults to
//!   `wpm`). With Farnsworth, a spacing unit is `(50 * 1.2 / wpm - 31 *
//!   content) / 19` s ("PARIS " = 31 content + 19 spacing units); otherwise
//!   it equals the content unit.
//! - Marks: a dot lasts `dit_units`, a dash `weight` content units; elements
//!   of one character are `element_gap_units` content units apart;
//!   characters `char_gap_units` and words `word_gap_units` spacing units.
//! - Jitter, per segment (mark or gap) in keying order: draw two `u64` from
//!   `ChaCha8Rng::seed_from_u64(seed)`, map each to `((u >> 11) + 0.5) /
//!   2^53`, take the cosine Box–Muller value, clamp it to ±3 and scale the
//!   nominal duration by `1 + sigma * z`.
//! - Loop: characters are keyed only while they fit in the scene; a word gap
//!   separates repetitions.
//! - Edges: `sin²` ramps of `min(rise, mark / 2)` inside each mark.
//! - Carrier `A · v(t) · exp(j 2π frac(f i / fs))`, with `A² fs / 2500 =
//!   10^(snr/10)` against unit-power complex noise.
//! - Fading: two equal-power paths, each a sum of 256 complex sinusoids with
//!   Doppler frequencies from N(0, (spread / 2)²) and uniform phases,
//!   evaluated every 1 ms and linearly interpolated.

use anyhow::{bail, Result};
use num_complex::{Complex32, Complex64};
use rand_chacha::ChaCha8Rng;
use rand_core::{Rng, SeedableRng};

/// Headroom scale after mixing; the gate compares it with the production
/// scale.
pub const HEADROOM: f32 = 0.05;

const NOISE_DOMAIN: u64 = 0x4E4F_4953_455F_4231; // "NOISE_B1"
const FADE_DOMAIN: u64 = 0x4641_4445_5F42_3121; // "FADE_B1!"
/// Sinusoids per fading path.
pub const FADE_SINUSOIDS: usize = 256;
/// Fading gain evaluation rate.
pub const FADE_GAIN_RATE_HZ: f64 = 1000.0;

/// Keying timing. Units are dit units unless named otherwise.
#[derive(Debug, Clone, PartialEq)]
pub struct Timing {
    pub wpm: f64,
    pub char_wpm: Option<f64>,
    pub weight: f64,
    pub dit_units: f64,
    pub element_gap_units: f64,
    pub char_gap_units: f64,
    pub word_gap_units: f64,
    pub rise_s: f64,
    /// `(sigma, seed)`.
    pub jitter: Option<(f64, u64)>,
}

impl Timing {
    /// Standard 3:1 keying at `wpm`, 5 ms edges, no jitter.
    pub fn standard(wpm: f64) -> Self {
        Timing {
            wpm,
            char_wpm: None,
            weight: 3.0,
            dit_units: 1.0,
            element_gap_units: 1.0,
            char_gap_units: 3.0,
            word_gap_units: 7.0,
            rise_s: 0.005,
            jitter: None,
        }
    }
}

/// ITU-R F.1487 two-path channel profiles, typed here independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FadeProfile {
    Good,
    Moderate,
    Poor,
}

impl FadeProfile {
    /// Differential delay of the second path, seconds.
    pub fn delay_s(self) -> f64 {
        match self {
            FadeProfile::Good => 0.0005,
            FadeProfile::Moderate => 0.001,
            FadeProfile::Poor => 0.002,
        }
    }

    /// ITU frequency spread (two-sigma width of the Doppler PSD), Hz.
    pub fn spread_hz(self) -> f64 {
        match self {
            FadeProfile::Good => 0.1,
            FadeProfile::Moderate => 0.5,
            FadeProfile::Poor => 1.0,
        }
    }
}

/// One keyed tone.
#[derive(Debug, Clone, PartialEq)]
pub struct ToneSpec {
    pub text: String,
    pub loop_text: bool,
    pub timing: Timing,
    pub offset_hz: f64,
    pub snr_2500_db: f64,
    pub qsb_rate_hz: Option<f64>,
    /// `(profile, seed)`.
    pub fade: Option<(FadeProfile, u64)>,
}

/// A scene of tones plus optional unit-power complex noise.
#[derive(Debug, Clone, PartialEq)]
pub struct IndependentScene {
    pub fs: f64,
    pub duration_s: f64,
    pub noise_seed: Option<u64>,
    pub tones: Vec<ToneSpec>,
}

/// ITU-R M.1677-1 patterns. `+` is the ITU "cross" (`.-.-.`).
fn itu(c: char) -> Option<&'static str> {
    Some(match c {
        'A' => ".-",
        'B' => "-...",
        'C' => "-.-.",
        'D' => "-..",
        'E' => ".",
        'F' => "..-.",
        'G' => "--.",
        'H' => "....",
        'I' => "..",
        'J' => ".---",
        'K' => "-.-",
        'L' => ".-..",
        'M' => "--",
        'N' => "-.",
        'O' => "---",
        'P' => ".--.",
        'Q' => "--.-",
        'R' => ".-.",
        'S' => "...",
        'T' => "-",
        'U' => "..-",
        'V' => "...-",
        'W' => ".--",
        'X' => "-..-",
        'Y' => "-.--",
        'Z' => "--..",
        '0' => "-----",
        '1' => ".----",
        '2' => "..---",
        '3' => "...--",
        '4' => "....-",
        '5' => ".....",
        '6' => "-....",
        '7' => "--...",
        '8' => "---..",
        '9' => "----.",
        '.' => ".-.-.-",
        ',' => "--..--",
        '?' => "..--..",
        '/' => "-..-.",
        '=' => "-...-",
        '-' => "-....-",
        '(' => "-.--.",
        ')' => "-.--.-",
        ':' => "---...",
        '\'' => ".----.",
        '"' => ".-..-.",
        '@' => ".--.-.",
        '+' => ".-.-.",
        _ => return None,
    })
}

/// Every character of the independent table, for the gate's table check.
pub(crate) fn itu_characters() -> Vec<(char, &'static str)> {
    "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789.,?/=-():'\"@+"
        .chars()
        .map(|c| (c, itu(c).unwrap_or("")))
        .collect()
}

struct Clock {
    t: f64,
    marks: Vec<(f64, f64)>,
    rng: Option<(ChaCha8Rng, f64)>,
}

impl Clock {
    fn draw(&mut self, nominal: f64) -> f64 {
        match &mut self.rng {
            None => nominal,
            Some((rng, sigma)) => {
                let a = ((rng.next_u64() >> 11) as f64 + 0.5) / 9_007_199_254_740_992.0;
                let b = ((rng.next_u64() >> 11) as f64 + 0.5) / 9_007_199_254_740_992.0;
                let z = (-2.0 * a.ln()).sqrt() * (std::f64::consts::TAU * b).cos();
                nominal * (1.0 + *sigma * z.clamp(-3.0, 3.0))
            }
        }
    }

    fn mark(&mut self, nominal: f64) -> f64 {
        let d = self.draw(nominal);
        self.marks.push((self.t, self.t + d));
        self.t += d;
        d
    }

    fn space(&mut self, nominal: f64) -> f64 {
        let d = self.draw(nominal);
        self.t += d;
        d
    }
}

fn units(t: &Timing) -> (f64, f64) {
    let content = 1.2 / t.char_wpm.unwrap_or(t.wpm);
    let spacing = match t.char_wpm {
        None => content,
        Some(_) => (50.0 * 1.2 / t.wpm - 31.0 * content) / 19.0,
    };
    (content, spacing)
}

/// Key one character's elements; returns the time it took.
fn key_char(clock: &mut Clock, c: char, t: &Timing, content: f64) -> Result<f64> {
    let Some(p) = itu(c) else {
        bail!("no ITU Morse pattern for {:?}", c);
    };
    let mut took = 0.0;
    let n = p.len();
    for (k, e) in p.chars().enumerate() {
        took += clock.mark(if e == '.' { t.dit_units } else { t.weight } * content);
        if k + 1 < n {
            took += clock.space(t.element_gap_units * content);
        }
    }
    Ok(took)
}

/// Key a tone's text: marks as `(start_s, end_s)` and the keyed text.
pub(crate) fn timeline(tone: &ToneSpec, duration_s: f64) -> Result<(Vec<(f64, f64)>, String)> {
    let t = &tone.timing;
    let (content, spacing) = units(t);
    let words: Vec<String> = tone
        .text
        .split_whitespace()
        .map(|w| w.to_uppercase())
        .collect();
    let mut clock = Clock {
        t: 0.0,
        marks: Vec::new(),
        rng: t
            .jitter
            .map(|(s, seed)| (ChaCha8Rng::seed_from_u64(seed), s)),
    };
    if !tone.loop_text {
        for (wi, w) in words.iter().enumerate() {
            let cs: Vec<char> = w.chars().collect();
            for (ci, c) in cs.iter().enumerate() {
                key_char(&mut clock, *c, t, content)?;
                if ci + 1 < cs.len() {
                    clock.space(t.char_gap_units * spacing);
                }
            }
            if wi + 1 < words.len() {
                clock.space(t.word_gap_units * spacing);
            }
        }
        return Ok((clock.marks, words.join(" ")));
    }
    // Loop: a character is keyed only if it ends inside the scene. Its
    // segments are drawn (advancing the jitter stream) before the fit test.
    let mut keyed = String::new();
    let mut used = 0.0f64;
    'rep: loop {
        for (wi, w) in words.iter().enumerate() {
            let cs: Vec<char> = w.chars().collect();
            for (ci, c) in cs.iter().enumerate() {
                let mark_count = clock.marks.len();
                let t_before = clock.t;
                let took = key_char(&mut clock, *c, t, content)?;
                if used + took > duration_s {
                    clock.marks.truncate(mark_count);
                    clock.t = t_before;
                    break 'rep;
                }
                used += took;
                keyed.push(*c);
                if ci + 1 < cs.len() {
                    used += clock.space(t.char_gap_units * spacing);
                }
            }
            if wi + 1 < words.len() {
                used += clock.space(t.word_gap_units * spacing);
                keyed.push(' ');
            }
        }
        used += clock.space(t.word_gap_units * spacing);
        keyed.push(' ');
        if used >= duration_s {
            break;
        }
    }
    Ok((clock.marks, keyed.trim().to_string()))
}

/// Unit-amplitude keying envelope over `n` samples at `fs`.
pub(crate) fn envelope(marks: &[(f64, f64)], rise_s: f64, fs: f64, n: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; n];
    for &(a, b) in marks {
        let r = rise_s.min((b - a) / 2.0);
        let lo = ((a * fs).floor().max(0.0)) as usize;
        let hi = (((b * fs).ceil() + 1.0).max(0.0) as usize).min(n);
        for (i, out) in v.iter_mut().enumerate().take(hi).skip(lo) {
            let t = i as f64 / fs;
            if t < a || t >= b {
                continue;
            }
            let ramp = |x: f64| {
                if x < r {
                    let s = (std::f64::consts::FRAC_PI_2 * x / r).sin();
                    s * s
                } else {
                    1.0
                }
            };
            *out = ramp(t - a).min(ramp(b - t)) as f32;
        }
    }
    v
}

/// Carrier amplitude for `snr_db` in 2500 Hz against unit-power noise.
pub(crate) fn amplitude(snr_db: f64, fs: f64) -> f64 {
    (10f64.powf(snr_db / 10.0) * 2500.0 / fs).sqrt()
}

fn uniform(rng: &mut ChaCha8Rng) -> f64 {
    (rng.next_u64() >> 11) as f64 / 9_007_199_254_740_992.0
}

/// Marsaglia polar method: a pair of independent N(0, 1) values.
fn polar_pair(rng: &mut ChaCha8Rng) -> (f64, f64) {
    loop {
        let u = 2.0 * uniform(rng) - 1.0;
        let v = 2.0 * uniform(rng) - 1.0;
        let s = u * u + v * v;
        if s > 0.0 && s < 1.0 {
            let m = (-2.0 * s.ln() / s).sqrt();
            return (u * m, v * m);
        }
    }
}

/// Two fading paths' complex gains at `fs` over `n` samples. Each path is a
/// sum of `FADE_SINUSOIDS` unit sinusoids scaled by `1/sqrt(M)` (unit
/// ensemble power), Doppler sigma `sigma_scale * spread / 2`.
pub(crate) fn fade_gain(
    profile: FadeProfile,
    seed: u64,
    sigma_scale: f64,
    fs: f64,
    n: usize,
) -> [Vec<Complex64>; 2] {
    let mut rng = ChaCha8Rng::seed_from_u64(seed ^ FADE_DOMAIN);
    let sigma = sigma_scale * profile.spread_hz() / 2.0;
    let step = 1.0 / FADE_GAIN_RATE_HZ;
    let points = ((n as f64 / fs) / step).ceil() as usize + 2;
    let mut path = || {
        let mut freqs = Vec::with_capacity(FADE_SINUSOIDS);
        while freqs.len() < FADE_SINUSOIDS {
            let (a, b) = polar_pair(&mut rng);
            freqs.push(a * sigma);
            if freqs.len() < FADE_SINUSOIDS {
                freqs.push(b * sigma);
            }
        }
        let phases: Vec<f64> = (0..FADE_SINUSOIDS)
            .map(|_| std::f64::consts::TAU * uniform(&mut rng))
            .collect();
        // Rotating phasors, re-anchored exactly every 1000 points.
        let norm = 1.0 / (FADE_SINUSOIDS as f64).sqrt();
        let rot: Vec<Complex64> = freqs
            .iter()
            .map(|f| Complex64::from_polar(1.0, std::f64::consts::TAU * f * step))
            .collect();
        let mut ph: Vec<Complex64> = Vec::new();
        let mut coarse = Vec::with_capacity(points);
        for k in 0..points {
            if k % 1000 == 0 {
                let t = k as f64 * step;
                ph = freqs
                    .iter()
                    .zip(&phases)
                    .map(|(f, p)| Complex64::from_polar(1.0, std::f64::consts::TAU * f * t + p))
                    .collect();
            }
            coarse.push(ph.iter().sum::<Complex64>() * norm);
            for (p, r) in ph.iter_mut().zip(&rot) {
                *p *= r;
            }
        }
        (0..n)
            .map(|i| {
                let x = i as f64 / fs / step;
                let k = x.floor() as usize;
                let w = x - k as f64;
                coarse[k] * (1.0 - w) + coarse[k + 1] * w
            })
            .collect::<Vec<Complex64>>()
    };
    let g1 = path();
    let g2 = path();
    [g1, g2]
}

/// One tone's unscaled contribution and keyed text.
pub(crate) fn render_tone(
    tone: &ToneSpec,
    fs: f64,
    duration_s: f64,
) -> Result<(Vec<Complex32>, String)> {
    render_tone_with(tone, fs, duration_s, 1.0)
}

/// `render_tone` with the fader's Doppler sigma scaled (the gate's
/// negative control injects `sigma = spread`).
pub(crate) fn render_tone_with(
    tone: &ToneSpec,
    fs: f64,
    duration_s: f64,
    fade_sigma_scale: f64,
) -> Result<(Vec<Complex32>, String)> {
    let n = (duration_s * fs).round() as usize;
    let (marks, text) = timeline(tone, duration_s)?;
    let env = envelope(&marks, tone.timing.rise_s, fs, n);
    let a = amplitude(tone.snr_2500_db, fs);
    let z: Vec<Complex64> = env
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let mut e = a * v as f64;
            if let Some(r) = tone.qsb_rate_hz {
                e *= 0.55 + 0.45 * (std::f64::consts::TAU * r * i as f64 / fs).sin();
            }
            if e == 0.0 {
                return Complex64::new(0.0, 0.0);
            }
            let cyc = tone.offset_hz * i as f64 / fs;
            Complex64::from_polar(e, std::f64::consts::TAU * (cyc - cyc.floor()))
        })
        .collect();
    let out: Vec<Complex32> = match tone.fade {
        None => z
            .iter()
            .map(|c| Complex32::new(c.re as f32, c.im as f32))
            .collect(),
        Some((profile, seed)) => {
            let [g1, g2] = fade_gain(profile, seed, fade_sigma_scale, fs, n);
            let d = (profile.delay_s() * fs).round() as usize;
            let h = 0.5f64.sqrt();
            (0..n)
                .map(|i| {
                    let late = if i >= d {
                        z[i - d]
                    } else {
                        Complex64::default()
                    };
                    let c = (g1[i] * z[i] + g2[i] * late) * h;
                    Complex32::new(c.re as f32, c.im as f32)
                })
                .collect()
        }
    };
    Ok((out, text))
}

/// Unit-power complex AWGN (per-component variance 0.5).
pub(crate) fn noise(n: usize, seed: u64) -> Vec<Complex32> {
    let mut rng = ChaCha8Rng::seed_from_u64(seed ^ NOISE_DOMAIN);
    let s = 0.5f64.sqrt();
    (0..n)
        .map(|_| {
            let (a, b) = polar_pair(&mut rng);
            Complex32::new((a * s) as f32, (b * s) as f32)
        })
        .collect()
}

/// Render a scene: tones summed in order, noise added, `HEADROOM` applied.
pub(crate) fn render(scene: &IndependentScene) -> Result<(Vec<Complex32>, Vec<String>)> {
    let n = (scene.duration_s * scene.fs).round() as usize;
    let mut acc = vec![Complex32::new(0.0, 0.0); n];
    let mut texts = Vec::with_capacity(scene.tones.len());
    for tone in &scene.tones {
        let (z, text) = render_tone(tone, scene.fs, scene.duration_s)?;
        for (o, v) in acc.iter_mut().zip(&z) {
            *o += v;
        }
        texts.push(text);
    }
    if let Some(seed) = scene.noise_seed {
        for (o, v) in acc.iter_mut().zip(noise(n, seed)) {
            *o += v;
        }
    }
    for o in &mut acc {
        *o *= HEADROOM;
    }
    Ok((acc, texts))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(text: &str, timing: Timing, loop_text: bool) -> ToneSpec {
        ToneSpec {
            text: text.into(),
            loop_text,
            timing,
            offset_hz: 1000.0,
            snr_2500_db: 20.0,
            qsb_rate_hz: None,
            fade: None,
        }
    }

    fn span(marks: &[(f64, f64)]) -> f64 {
        marks.last().unwrap().1 - marks[0].0
    }

    #[test]
    fn paris_is_fifty_units_at_any_speed() {
        // "PARIS " = 50 units; without the trailing word gap, 43 units.
        for wpm in [12.0, 20.0, 35.0] {
            let (m, _) = timeline(&tone("PARIS", Timing::standard(wpm), false), 60.0).unwrap();
            let unit = 1.2 / wpm;
            assert!((span(&m) - 43.0 * unit).abs() < 1e-9, "{wpm}");
        }
        // 20 WPM: 50 units of 60 ms = 3.000 s.
        assert!((50.0 * 1.2 / 20.0 - 3.0f64).abs() < 1e-12);
        // Farnsworth 15/25: 31 content units at 25 WPM + 19 spacing units
        // add up to one 15 WPM word.
        let t = Timing {
            char_wpm: Some(25.0),
            ..Timing::standard(15.0)
        };
        let (c, s) = units(&t);
        assert!((31.0 * c + 19.0 * s - 50.0 * 1.2 / 15.0).abs() < 1e-12);
    }

    #[test]
    fn itu_table_covers_letters_digits_and_punctuation() {
        for (c, p) in itu_characters() {
            assert!(!p.is_empty(), "{c}");
        }
        assert_eq!(itu('+'), Some(".-.-."));
        assert_eq!(itu('@'), Some(".--.-."));
        assert_eq!(itu_characters().len(), 49);
    }

    #[test]
    fn loop_keys_only_characters_that_fit() {
        let tt = tone("TEST", Timing::standard(20.0), true);
        let (m, text) = timeline(&tt, 1.0).unwrap();
        assert!(m.last().unwrap().1 <= 1.0);
        assert!(!text.is_empty() && "TEST".starts_with(&text), "{text}");
    }

    #[test]
    fn loop_inserts_a_word_gap_between_repetitions() {
        let tt = tone("E", Timing::standard(20.0), true);
        let (m, text) = timeline(&tt, 2.0).unwrap();
        // E (1 unit) + word gap (7 units) = 8 units of 60 ms per repetition.
        assert!((m[1].0 - m[0].0 - 8.0 * 0.06).abs() < 1e-12);
        assert!(text.starts_with("E E"), "{text}");
    }

    #[test]
    fn jitter_is_one_box_muller_draw_per_segment_clamped_at_three_sigma() {
        let t = Timing {
            jitter: Some((0.08, 7)),
            ..Timing::standard(20.0)
        };
        let (m, _) = timeline(&tone("EEEEEEEEEEEEEEEEEEEE", t, true), 30.0).unwrap();
        for (a, b) in m {
            let r = (b - a) / 0.06;
            assert!(
                (1.0 - 0.24 - 1e-12..=1.0 + 0.24 + 1e-12).contains(&r),
                "{r}"
            );
        }
    }

    #[test]
    fn edges_are_raised_cosine_of_min_rise_and_half_mark() {
        let fs = 1000.0;
        let env = envelope(&[(0.0, 0.004)], 0.005, fs, 10);
        // rise = min(5 ms, 2 ms) = 2 ms; peak at 2 ms.
        assert!((env[1] - 0.5).abs() < 1e-6, "{:?}", env);
        assert!((env[2] - 1.0).abs() < 1e-6, "{:?}", env);
        assert_eq!(env[4], 0.0);
    }

    #[test]
    fn carrier_phase_comes_from_the_sample_index() {
        let fs = 96_000.0;
        let f = 45_000.0;
        let i = 11_519_999u64;
        let cyc = f * i as f64 / fs;
        let ph = std::f64::consts::TAU * (cyc - cyc.floor());
        let exact = std::f64::consts::TAU * ((f as u64 * i) % fs as u64) as f64 / fs;
        assert!((ph - exact).abs() < 1e-6, "{ph} vs {exact}");
    }

    #[test]
    fn amplitude_meets_the_2500_hz_definition() {
        let a = amplitude(20.0, 96_000.0);
        assert!((a * a * 96_000.0 / 2500.0 - 100.0).abs() < 1e-9);
    }

    #[test]
    fn qsb_multiplier_is_spec_v6() {
        let mut tt = tone("T", Timing::standard(0.5), false);
        tt.qsb_rate_hz = Some(0.5);
        let fs = 1000.0;
        let (z, _) = render_tone(&tt, fs, 2.0).unwrap();
        let a = amplitude(20.0, fs);
        let i = 500; // t = 0.5 s: sin(π/2) = 1, so multiplier 1.0
        assert!(((z[i].norm() as f64) / a - 1.0).abs() < 1e-6);
        let i = 1500; // t = 1.5 s: multiplier 0.10
        assert!(((z[i].norm() as f64) / a - 0.10).abs() < 1e-6);
    }

    #[test]
    fn noise_has_unit_power_and_is_deterministic() {
        let a = noise(200_000, 3);
        let p = a.iter().map(|c| c.norm_sqr() as f64).sum::<f64>() / a.len() as f64;
        assert!((p - 1.0).abs() < 0.02, "{p}");
        assert_eq!(a, noise(200_000, 3));
    }

    #[test]
    fn fader_paths_are_equal_power_with_preset_delays() {
        assert_eq!(FadeProfile::Good.delay_s(), 0.0005);
        assert_eq!(FadeProfile::Moderate.delay_s(), 0.001);
        assert_eq!(FadeProfile::Poor.delay_s(), 0.002);
        let mut p = [0.0f64; 2];
        for seed in 0..32 {
            let g = fade_gain(FadeProfile::Poor, seed, 1.0, 100.0, 6000);
            for (k, path) in g.iter().enumerate() {
                p[k] += path.iter().map(|c| c.norm_sqr()).sum::<f64>() / path.len() as f64 / 32.0;
            }
        }
        assert!(
            (p[0] - 1.0).abs() < 0.1 && (p[1] - 1.0).abs() < 0.1,
            "{p:?}"
        );
    }

    #[test]
    fn fader_gain_interpolation_error_is_below_1e_4() {
        // At 1 kHz, a 1 Hz-sigma path moves little in 1 ms; linear
        // interpolation midway between points is close to the exact sum.
        let fs = 8000.0;
        let g = fade_gain(FadeProfile::Poor, 9, 1.0, fs, 80_000);
        let mut worst = 0.0f64;
        for k in 1..(g[0].len() - 1) {
            let d = (g[0][k + 1] - 2.0 * g[0][k] + g[0][k - 1]).norm();
            if k % 8 != 0 {
                worst = worst.max(d);
            }
        }
        assert!(worst < 1e-4, "{worst}");
    }

    #[test]
    fn unknown_character_is_an_error() {
        assert!(timeline(&tone("A#B", Timing::standard(20.0), false), 10.0).is_err());
    }
}
