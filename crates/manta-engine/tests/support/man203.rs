//! MAN-203 test support: scenes with a receiver impairment injected, and
//! the scorers the conditioner acceptance tests share. Scenes follow the
//! `manta_testkit::sensitivity` convention: 96 kS/s, centre 14 MHz, 25 WPM,
//! `render_noise` at 0.05 rms per complex sample, and a hit is a spot with
//! the right call within 150 Hz. Trials start at 1 (trial 0 carries five
//! A-prefix calls that never spot, MAN-203 D13).
#![allow(dead_code)]

use manta_engine::{decode_samples, PipelineConfig};
use manta_testkit::scene::SignalSpec;
use manta_testkit::sensitivity::{
    compose, gain_for_snr_2500, payload, render_noise, render_series, render_station, series_calls,
    series_signals, Condition, CENTER_FREQ_HZ, SAMPLE_RATE_HZ,
};
use num_complex::Complex32;

/// The noise seed every MAN-203 scene uses.
pub const SEED: u64 = 203;
/// Scene length, seconds.
pub const DURATION_S: f64 = 30.0;
/// A hit is a spot of the right call within this distance, Hz.
pub const HIT_HZ: f64 = 150.0;

/// One station of a scene: its call and RF frequency.
#[derive(Debug, Clone)]
pub struct Station {
    pub call: String,
    pub freq_hz: f64,
}

/// A rendered scene and the stations in it.
pub struct Scene {
    pub samples: Vec<Complex32>,
    pub stations: Vec<Station>,
}

/// Adds the constant `dc` to every sample: a receiver DC offset.
pub fn add_dc(x: &mut [Complex32], dc: Complex32) {
    for s in x.iter_mut() {
        *s += dc;
    }
}

/// Sets `x[i] = v` for each `(i, v)`: isolated impulses.
pub fn add_impulses(x: &mut [Complex32], at: &[(usize, Complex32)]) {
    for &(i, v) in at {
        x[i] = v;
    }
}

/// `rate_hz` impulses per second of value `v`, for `duration_s` seconds
/// from 0.5 s, at evenly spaced sample indices.
pub fn impulse_train(rate_hz: f64, duration_s: f64, v: Complex32) -> Vec<(usize, Complex32)> {
    let n = (rate_hz * duration_s).round() as usize;
    (0..n)
        .map(|k| {
            (
                ((0.5 + k as f64 / rate_hz) * SAMPLE_RATE_HZ).round() as usize,
                v,
            )
        })
        .collect()
}

/// The conditioning log a channelizer running `cfg` writes over `x`: the
/// chain decode builds, run standalone so a test can read its counts.
/// Decode also feeds `filter_len()` zeros of lead-in padding first, so its
/// span indices and one-second block boundaries sit that many samples
/// later than here.
pub fn condition_log(
    x: &[Complex32],
    cfg: &manta_engine::ConditioningConfig,
) -> manta_dsp::condition::ConditioningLog {
    let mut ch =
        manta_dsp::channelizer::Channelizer::with_conditioning(SAMPLE_RATE_HZ, CENTER_FREQ_HZ, cfg)
            .expect("a supported sample rate");
    ch.process(x);
    ch.conditioning().expect("a stage is on").clone()
}

/// One station (`series_calls(trial)[0]`) at `offset_hz` and `snr_db`
/// (2500 Hz reference), plus the trial's noise and a DC offset
/// `(dc, 0.7 dc)`.
pub fn near_centre_scene(offset_hz: f64, snr_db: f64, dc: f32, trial: u32) -> Scene {
    let call = series_calls(trial)[0].clone();
    let mut sig: SignalSpec = series_signals(Condition::Awgn, 25.0, trial, SEED)
        .into_iter()
        .next()
        .expect("a series has ten stations");
    sig.text = payload(&call);
    sig.offset_hz = offset_hz;
    let (clean, _) = render_station(&sig, DURATION_S).expect("render the station");
    let noise = render_noise(DURATION_S, SEED, trial).expect("render the noise");
    let mut samples = compose(&clean, &noise, gain_for_snr_2500(snr_db));
    add_dc(&mut samples, Complex32::new(dc, 0.7 * dc));
    Scene {
        samples,
        stations: vec![Station {
            call,
            freq_hz: CENTER_FREQ_HZ + offset_hz,
        }],
    }
}

/// The trial's ten series stations at `snr_db`, plus its noise.
pub fn series_scene(snr_db: f64, trial: u32) -> Scene {
    let signals = series_signals(Condition::Awgn, 25.0, trial, SEED);
    let (clean, _) = render_series(&signals, DURATION_S).expect("render the series");
    let noise = render_noise(DURATION_S, SEED, trial).expect("render the noise");
    let samples = compose(&clean, &noise, gain_for_snr_2500(snr_db));
    let stations = series_calls(trial)
        .into_iter()
        .zip(&signals)
        .map(|(call, s)| Station {
            call,
            freq_hz: CENTER_FREQ_HZ + s.offset_hz,
        })
        .collect();
    Scene { samples, stations }
}

/// The decode result a scorer reads: spots as `(call, freq)`. "No signal
/// found" is an empty result, not an error.
pub struct Outcome {
    pub spots: Vec<(String, f64)>,
}

/// Decodes `x` with `cfg`.
pub fn decode(x: &[Complex32], cfg: &PipelineConfig) -> Outcome {
    match decode_samples(x, SAMPLE_RATE_HZ, CENTER_FREQ_HZ, cfg) {
        Ok(r) => Outcome {
            spots: r
                .spots
                .iter()
                .map(|s| (s.callsign.clone(), s.freq_hz))
                .collect(),
        },
        Err(e) if e.to_string().contains("no signal found") => Outcome { spots: Vec::new() },
        Err(e) => panic!("decode failed: {e:#}"),
    }
}

/// Stations of `scene` the outcome spotted.
pub fn found(scene: &Scene, out: &Outcome) -> usize {
    scene
        .stations
        .iter()
        .filter(|st| {
            out.spots
                .iter()
                .any(|(c, f)| *c == st.call && (f - st.freq_hz).abs() <= HIT_HZ)
        })
        .count()
}

/// The calls of `scene` the outcome spotted, sorted.
pub fn found_calls(scene: &Scene, out: &Outcome) -> Vec<String> {
    let mut calls: Vec<String> = scene
        .stations
        .iter()
        .filter(|st| {
            out.spots
                .iter()
                .any(|(c, f)| *c == st.call && (f - st.freq_hz).abs() <= HIT_HZ)
        })
        .map(|st| st.call.clone())
        .collect();
    calls.sort();
    calls
}

/// Applies a receiver's Q gain and quadrature phase error. The I arm is unchanged.
pub fn add_iq_imbalance(x: &mut [Complex32], gain_ratio: f64, phase_deg: f64) {
    let (sin, cos) = phase_deg.to_radians().sin_cos();
    for s in x {
        s.im = (gain_ratio * (f64::from(s.im) * cos + f64::from(s.re) * sin)) as f32;
    }
}

/// A controlled tone captured separately from a keyed station scene. Its
/// frequency intentionally misses the declared tone by 13.7 Hz.
pub fn iq_tone_capture(gain_ratio: f64, phase_deg: f64) -> Vec<Complex32> {
    let mut samples: Vec<_> = (0..5 * SAMPLE_RATE_HZ as usize)
        .map(|i| {
            let phase = std::f64::consts::TAU * 12_013.7 * i as f64 / SAMPLE_RATE_HZ;
            Complex32::new(phase.cos() as f32, phase.sin() as f32)
        })
        .collect();
    add_iq_imbalance(&mut samples, gain_ratio, phase_deg);
    samples
}

/// Spots in an otherwise empty conjugate-image window. A wrong call at
/// the mirror is still a bogus spot, while a real station there is not.
pub fn mirror_spots(scene: &Scene, out: &Outcome) -> usize {
    out.spots
        .iter()
        .filter(|(_, freq)| {
            scene.stations.iter().any(|station| {
                let mirror = 2.0 * CENTER_FREQ_HZ - station.freq_hz;
                (*freq - mirror).abs() <= HIT_HZ
                    && !scene
                        .stations
                        .iter()
                        .any(|real| (real.freq_hz - mirror).abs() <= HIT_HZ)
            })
        })
        .count()
}

/// The historical IQ-image experiment: one 45 dB station at +12056.25 Hz,
/// five 8 dB stations elsewhere, and no station at the strong one's mirror.
pub fn iq_scene(trial: u32) -> Scene {
    let offsets = [12_056.25, -30_000.0, -20_000.0, -6_000.0, 6_000.0, 30_000.0];
    let signals: Vec<_> = series_signals(Condition::Awgn, 25.0, trial, SEED)
        .into_iter()
        .zip(offsets)
        .enumerate()
        .map(|(i, (mut signal, offset))| {
            signal.offset_hz = offset;
            signal.snr_2500_db = if i == 0 { 45.0 } else { 8.0 };
            signal
        })
        .collect();
    let (clean, _) = render_series(&signals, DURATION_S).expect("render IQ experiment stations");
    let noise = render_noise(DURATION_S, SEED, trial).expect("render IQ experiment noise");
    let samples = compose(&clean, &noise, 1.0);
    let stations = series_calls(trial)
        .into_iter()
        .zip(offsets)
        .map(|(call, offset)| Station {
            call,
            freq_hz: CENTER_FREQ_HZ + offset,
        })
        .collect();
    Scene { samples, stations }
}
