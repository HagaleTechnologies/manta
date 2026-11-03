//! MAN-203 test support: scenes with a receiver impairment injected, and
//! the scorers the conditioner acceptance tests share. Scenes follow the
//! `manta_testkit::sensitivity` convention: 96 kS/s, centre 14 MHz, 25 WPM,
//! `render_noise` at 0.05 rms per complex sample, and a hit is a spot with
//! the right call within 150 Hz. Trials start at 1 (trial 0 carries five
//! A-prefix calls that never spot, MAN-203 D13).
#![allow(dead_code)]

use manta_decode::events::DecoderEvent;
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

/// The decode result a scorer reads: spots as `(call, freq)` and the
/// frequencies of every promoted track. "No signal found" is an empty
/// result, not an error.
pub struct Outcome {
    pub spots: Vec<(String, f64)>,
    pub promoted_hz: Vec<f64>,
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
            promoted_hz: r
                .events
                .iter()
                .filter_map(|e| match e {
                    DecoderEvent::TrackPromoted { freq_hz, .. } => Some(*freq_hz),
                    _ => None,
                })
                .collect(),
        },
        Err(e) if e.to_string().contains("no signal found") => Outcome {
            spots: Vec::new(),
            promoted_hz: Vec::new(),
        },
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

/// Promoted tracks within 100 Hz of the centre and more than 100 Hz from
/// every station: tracks a DC offset grew.
pub fn false_dc_tracks(scene: &Scene, out: &Outcome) -> usize {
    out.promoted_hz
        .iter()
        .filter(|&&f| {
            (f - CENTER_FREQ_HZ).abs() <= 100.0
                && scene
                    .stations
                    .iter()
                    .all(|st| (f - st.freq_hz).abs() > 100.0)
        })
        .count()
}
