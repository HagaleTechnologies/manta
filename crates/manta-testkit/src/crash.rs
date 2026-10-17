//! Impulsive-noise ("static crash") generator and the VC1-VC3 vectors that
//! gate the crash filter (MAN-312, SPEC §2.7).
//!
//! MAN-312: a separate adder rather than a `SignalSpec` field, so no
//! existing scene or golden vector changes. A crash is complex white
//! Gaussian noise added to an already-rendered scene (after
//! `MASTER_SCALE`), drawn through the crate's ChaCha8 `gaussian_pair`/`u01`
//! like every other generator here (pinned decision 2).
//!
//! The generator and its seeds are pinned. Every MAN-312 number (the VC1
//! promotion counts, VC2's dit counts and 24 crashes, VC3's 47 crashes and
//! its promoted/validated/bogus counts, and the detector constants measured
//! from them) was measured with `add_crash`, `crash_train` and the seeds
//! below. Changing a draw, its order, a conversion or a seed invalidates
//! those numbers. `dense_scene` is the exception: the prototype drew its
//! WPM/SNR with a local xorshift, so its tests assert criteria, not counts.

use crate::scene::{render_scene, SignalSpec, MASTER_SCALE};
use crate::vectors::{
    render, v1, v8w, write_rendered_fixture_set, Manifest, RenderedVector, VectorSpec,
};
use anyhow::Result;
use num_complex::Complex32;
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;
use std::path::Path;

/// One additive broadband crash: complex white Gaussian noise `level_db`
/// above the scene's unit AWGN power (after MASTER_SCALE), `width_ms` long.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Crash {
    pub start_s: f64,
    pub width_ms: f64,
    pub level_db: f64,
    pub seed: u64,
}

/// Add one crash to `iq` (sampled at `fs`), clipped to the buffer. The
/// noise power is `MASTER_SCALE^2 * 10^(level_db/10)`, split evenly
/// between I and Q; one `gaussian_pair` per sample, (I, Q).
pub fn add_crash(iq: &mut [Complex32], fs: f64, c: &Crash) {
    let amb = (MASTER_SCALE as f64).powi(2);
    let sigma = (amb * 10f64.powf(c.level_db / 10.0) / 2.0).sqrt();
    let start = ((c.start_s * fs).round() as usize).min(iq.len());
    let end = (start + (c.width_ms * 1e-3 * fs).round() as usize).min(iq.len());
    let mut rng = ChaCha8Rng::seed_from_u64(c.seed);
    for s in &mut iq[start..end] {
        let (a, b) = crate::gaussian_pair(&mut rng);
        s.re += (sigma * a) as f32;
        s.im += (sigma * b) as f32;
    }
}

/// Crash train: widths 1-10 ms, +20..+60 dB, every 1-4 s. Draw order per
/// crash: width, level, interval (all `crate::u01`); crash i gets seed 1000 + i.
pub fn crash_train(seed: u64, first_s: f64, end_s: f64) -> Vec<Crash> {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let (mut t, mut i, mut out) = (first_s, 0u64, Vec::new());
    while t < end_s {
        let w = 1.0 + 9.0 * crate::u01(&mut rng);
        let db = 20.0 + 40.0 * crate::u01(&mut rng);
        out.push(Crash {
            start_s: t,
            width_ms: w,
            level_db: db,
            seed: 1000 + i,
        });
        t += 1.0 + 3.0 * crate::u01(&mut rng);
        i += 1;
    }
    out
}

/// VC1 scene length, seconds.
const VC1_DURATION_S: f64 = 10.0;
/// VC1's single crash starts here, mid-scene.
const VC1_CRASH_START_S: f64 = 5.0;
/// VC1 noise seed at 96 kHz.
const VC1_NOISE_SEED_96K: u64 = 0x1234;
/// VC1 noise seed at every other rate (48/192 kHz, and the 12 kHz base of
/// the KiwiSDR-shaped scene).
const VC1_NOISE_SEED_OTHER: u64 = 0x99;
/// VC1 crash seed at 96 kHz.
const VC1_CRASH_SEED_96K: u64 = 77;
/// VC1 crash seed at every other rate.
const VC1_CRASH_SEED_OTHER: u64 = 5;

/// VC3's pinned crash-train seed.
pub const VC3_CRASH_SEED: u64 = 0x3333;
/// The residual `kiwi_like_96k`'s measurements used, dB below
/// `MASTER_SCALE^2`.
pub const KIWI_RESIDUAL_DB: f64 = 100.0;
/// The residual seed `kiwi_like_96k`'s measurements used.
pub const KIWI_RESIDUAL_SEED: u64 = 11;

/// VC1 "crash on a signal-free band": 10 s of unit AWGN, no signals. Noise
/// seed `0x1234` at 96 kHz and `0x99` at any other rate.
pub fn vc1_scene(fs: f64) -> Vec<Complex32> {
    let seed = if fs == 96_000.0 {
        VC1_NOISE_SEED_96K
    } else {
        VC1_NOISE_SEED_OTHER
    };
    render_scene(&[], fs, VC1_DURATION_S, Some(seed))
        .expect("a signal-free scene has no text to key")
        .0
}

/// VC1's (width_ms, level_db) cases, in the order of the MAN-312
/// measurement table (the crash-free row excluded). Each case is one crash
/// from `vc1_crash`.
pub fn vc1_cases() -> Vec<(f64, f64)> {
    vec![
        (5.0, 6.0),
        (5.0, 20.0),
        (5.0, 30.0),
        (5.0, 40.0),
        (5.0, 50.0),
        (5.0, 60.0),
        (1.0, 40.0),
        (1.0, 60.0),
        (60.0, 20.0),
        (500.0, 20.0),
    ]
}

/// VC1's crash for one case: at 5.0 s, seed 77 at 96 kHz and 5 at any
/// other rate.
pub fn vc1_crash(fs: f64, width_ms: f64, level_db: f64) -> Crash {
    Crash {
        start_s: VC1_CRASH_START_S,
        width_ms,
        level_db,
        seed: if fs == 96_000.0 {
            VC1_CRASH_SEED_96K
        } else {
            VC1_CRASH_SEED_OTHER
        },
    }
}

/// VC2 "crash mid-character": V1's W1AW at +15 dB for 60 s, plus 5 ms
/// crashes at `level_db` every 2.37 s from 3.1 s while `t < duration - 1`
/// (24 crashes; crash i has seed 100 + i).
pub fn vc2(level_db: f64) -> (VectorSpec, Vec<Crash>) {
    let mut spec = VectorSpec {
        name: "vc2",
        duration_s: 60.0,
        ..v1()
    };
    spec.signals[0].snr_2500_db = 15.0;
    let mut crashes = Vec::new();
    let mut i = 0u64;
    loop {
        let t = 3.1 + 2.37 * i as f64;
        if t >= spec.duration_s - 1.0 {
            break;
        }
        crashes.push(Crash {
            start_s: t,
            width_ms: 5.0,
            level_db,
            seed: 100 + i,
        });
        i += 1;
    }
    (spec, crashes)
}

/// VC3 "pileup with crashes": V8w plus `crash_train(0x3333, 1.5, 119.5)`
/// (47 crashes).
pub fn vc3() -> (VectorSpec, Vec<Crash>) {
    let spec = VectorSpec {
        name: "vc3",
        ..v8w()
    };
    (spec, crash_train(VC3_CRASH_SEED, 1.5, 119.5))
}

/// Upsampling factor from the 12 kS/s KiwiSDR stream to 96 kS/s.
const KIWI_UP: usize = 8;
/// Interpolation filter length (odd, so the delay is a whole sample).
const KIWI_TAPS: usize = 511;
/// Interpolation filter cutoff at the 96 kS/s output rate, Hz.
const KIWI_FC_HZ: f64 = 5_000.0;
const KIWI_OUT_FS: f64 = 96_000.0;

/// The 511-tap Blackman-windowed sinc lowpass (fc 5 kHz at 96 kS/s),
/// normalised to a DC gain of exactly 8 (the zero-stuffing loss).
fn kiwi_taps() -> Vec<f64> {
    use std::f64::consts::{PI, TAU};
    let centre = (KIWI_TAPS - 1) as f64 / 2.0;
    let fc = KIWI_FC_HZ / KIWI_OUT_FS; // cycles per sample
    let span = (KIWI_TAPS - 1) as f64;
    let mut h: Vec<f64> = (0..KIWI_TAPS)
        .map(|k| {
            let x = k as f64 - centre;
            let sinc = if x == 0.0 {
                2.0 * fc
            } else {
                (TAU * fc * x).sin() / (PI * x)
            };
            let ph = TAU * k as f64 / span;
            let w = 0.42 - 0.5 * ph.cos() + 0.08 * (2.0 * ph).cos();
            sinc * w
        })
        .collect();
    let sum: f64 = h.iter().sum();
    for t in &mut h {
        *t *= KIWI_UP as f64 / sum;
    }
    h
}

/// A KiwiSDR-shaped 96 kS/s stream from a 12 kS/s scene: zero-stuff x8,
/// filter with a 511-tap Blackman-windowed sinc (fc 5 kHz at 96 kHz, gain
/// 8), then add ChaCha8 white noise `residual_db` below `MASTER_SCALE^2`
/// (one `gaussian_pair` per sample, (I, Q), from `seed`). The filter's
/// 255-sample delay is removed, so output sample `8n` lines up with input
/// sample `n` and the output is exactly 8x the input length. The
/// measurements used `KIWI_RESIDUAL_DB` (100 dB) and `KIWI_RESIDUAL_SEED`
/// (11). Add a crash to `iq_12k` before calling this, as the measurements
/// did.
pub fn kiwi_like_96k(iq_12k: &[Complex32], residual_db: f64, seed: u64) -> Vec<Complex32> {
    let h = kiwi_taps();
    let delay = (KIWI_TAPS - 1) / 2;
    let len = iq_12k.len() * KIWI_UP;
    let mut out = Vec::with_capacity(len);
    for n in 0..len {
        // y[n] = sum_k h[k] * up[n + delay - k], k ascending; only every
        // 8th `up` sample is non-zero, so start at the first k that lands
        // on one and step by 8 (bit-identical to the dense sum).
        let j0 = n + delay;
        let (mut re, mut im) = (0.0f64, 0.0f64);
        let mut k = j0 % KIWI_UP;
        while k < KIWI_TAPS && k <= j0 {
            let j = j0 - k;
            if j < len {
                let x = iq_12k[j / KIWI_UP];
                re += h[k] * x.re as f64;
                im += h[k] * x.im as f64;
            }
            k += KIWI_UP;
        }
        out.push(Complex32::new(re as f32, im as f32));
    }
    let amb = (MASTER_SCALE as f64).powi(2);
    let sigma = (amb * 10f64.powf(-residual_db / 10.0) / 2.0).sqrt();
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    for s in &mut out {
        let (a, b) = crate::gaussian_pair(&mut rng);
        s.re += (sigma * a) as f32;
        s.im += (sigma * b) as f32;
    }
    out
}

/// Seconds `dense_scene` renders and then drops.
const DENSE_LEAD_IN_S: f64 = 2.0;
/// Salt for `dense_scene`'s callsign draw, so it does not reuse the
/// WPM/SNR stream.
const DENSE_CALLS_SALT: u64 = 0xCA11_5167;
/// Salt for `dense_scene`'s noise seed.
const DENSE_NOISE_SALT: u64 = 0x4015_E000;

/// A dense band joined mid-traffic: a signal every `spacing_hz` from
/// -45% to +45% of `fs`, each keying `CQ TEST <call> <call> TEST` in a
/// loop at a ChaCha8-drawn speed (20-35 WPM) and SNR (10-30 dB in 2500
/// Hz); per signal the draws are WPM then SNR, from `seed`. Callsigns are
/// unique per scene. Unit AWGN, `MASTER_SCALE`, no jitter or fading.
///
/// It renders `duration_s + 2.0` s and drops the first 2 s, so the result
/// is exactly `duration_s * fs` samples of a band already in progress.
/// Without the drop every signal keys its first dah at t = 0: the whole
/// band rises together in the first hops, a synchronized-start artifact no
/// real band has, and the crash detector counts it (at 12 kHz with 100 Hz
/// spacing, 3 crashes at 0.23, 0.33 and 0.67 s).
///
/// The MAN-312 prototype drew WPM/SNR with a local xorshift, so this
/// ChaCha8 version's draws differ from the measured dense-band rows; assert
/// the criteria, not the prototype's counts.
pub fn dense_scene(fs: f64, spacing_hz: f64, duration_s: f64, seed: u64) -> Vec<Complex32> {
    let half = 0.45 * fs;
    let n_signals = ((2.0 * half) / spacing_hz + 1e-9).floor() as usize + 1;
    let calls = crate::callsigns::unique_calls(seed ^ DENSE_CALLS_SALT, n_signals);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let signals: Vec<SignalSpec> = calls
        .iter()
        .enumerate()
        .map(|(i, call)| {
            let wpm = 20.0 + 15.0 * crate::u01(&mut rng);
            let snr = 10.0 + 20.0 * crate::u01(&mut rng);
            SignalSpec {
                text: format!("CQ TEST {call} {call} TEST"),
                loop_text: true,
                wpm: wpm as f32,
                offset_hz: -half + i as f64 * spacing_hz,
                snr_2500_db: snr as f32,
                jitter: None,
                qsb: None,
                watterson: None,
                char_wpm: None,
                weight: 3.0,
                dit_units: 1.0,
                element_gap_units: 1.0,
                char_gap_units: 3.0,
                word_gap_units: 7.0,
                rise_ms: 5.0,
            }
        })
        .collect();
    let (mut samples, _) = render_scene(
        &signals,
        fs,
        duration_s + DENSE_LEAD_IN_S,
        Some(seed ^ DENSE_NOISE_SALT),
    )
    .expect("dense-scene text is plain letters, digits and spaces");
    let lead = ((DENSE_LEAD_IN_S * fs).round() as usize).min(samples.len());
    samples.drain(..lead);
    samples.truncate((duration_s * fs).round() as usize);
    samples
}

/// `render(spec)` with `crashes` added, in order, after `MASTER_SCALE`.
pub fn render_with_crashes(spec: &VectorSpec, crashes: &[Crash]) -> Result<RenderedVector> {
    let mut rendered = render(spec)?;
    for c in crashes {
        add_crash(&mut rendered.samples, spec.fs, c);
    }
    Ok(rendered)
}

/// `write_fixture_set` for a crash vector: the same `<name>.wav`,
/// `<name>.json` and `<name>.manifest.json`, from `render_with_crashes`,
/// with the crashes recorded in the manifest.
pub fn write_crash_fixture_set(
    spec: &VectorSpec,
    crashes: &[Crash],
    dir: &Path,
) -> Result<Manifest> {
    let rendered = render_with_crashes(spec, crashes)?;
    write_rendered_fixture_set(spec, rendered, crashes.to_vec(), dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vectors::write_fixture_set;

    fn zeros(n: usize) -> Vec<Complex32> {
        vec![Complex32::new(0.0, 0.0); n]
    }

    #[test]
    fn same_seed_same_samples() {
        let fs = 96_000.0;
        let c = Crash {
            start_s: 0.01,
            width_ms: 5.0,
            level_db: 40.0,
            seed: 77,
        };
        let mut a = zeros(4_800);
        let mut b = zeros(4_800);
        add_crash(&mut a, fs, &c);
        add_crash(&mut b, fs, &c);
        assert_eq!(a, b);
        assert!(a.iter().any(|s| s.norm_sqr() > 0.0), "crash added nothing");

        let mut other = zeros(4_800);
        add_crash(&mut other, fs, &Crash { seed: 78, ..c });
        assert_ne!(a, other, "a different seed must give different samples");

        assert_eq!(crash_train(7, 1.5, 60.0), crash_train(7, 1.5, 60.0));
        assert_eq!(vc1_scene(48_000.0), vc1_scene(48_000.0));
    }

    #[test]
    fn vc2_has_24_crashes() {
        let (spec, crashes) = vc2(40.0);
        assert_eq!(spec.duration_s, 60.0);
        assert_eq!(spec.signals[0].snr_2500_db, 15.0);
        assert_eq!(crashes.len(), 24);
        assert!((crashes[0].start_s - 3.1).abs() < 1e-9);
        assert!((crashes[23].start_s - 57.61).abs() < 1e-9);
        for (i, c) in crashes.iter().enumerate() {
            assert_eq!(c.width_ms, 5.0);
            assert_eq!(c.level_db, 40.0);
            assert_eq!(c.seed, 100 + i as u64);
        }
    }

    #[test]
    fn vc3_has_47_crashes() {
        let (spec, crashes) = vc3();
        assert_eq!(spec.name, "vc3");
        assert_eq!(spec.signals.len(), 50);
        assert_eq!(crashes.len(), 47);
        assert_eq!(crashes[0].start_s, 1.5);
        for (i, c) in crashes.iter().enumerate() {
            assert!((1.0..=10.0).contains(&c.width_ms), "{c:?}");
            assert!((20.0..=60.0).contains(&c.level_db), "{c:?}");
            assert!(c.start_s < 119.5, "{c:?}");
            assert_eq!(c.seed, 1000 + i as u64);
        }
    }

    #[test]
    fn burst_power_within_5_percent() {
        let fs = 96_000.0;
        let c = Crash {
            start_s: 0.1,
            width_ms: 100.0,
            level_db: 20.0,
            seed: 3,
        };
        let mut iq = zeros(28_800);
        add_crash(&mut iq, fs, &c);
        let burst = &iq[9_600..19_200];
        let p = burst.iter().map(|s| s.norm_sqr() as f64).sum::<f64>() / burst.len() as f64;
        let want = (MASTER_SCALE as f64).powi(2) * 100.0;
        assert!(
            (p / want - 1.0).abs() < 0.05,
            "burst power {p}, want {want}"
        );
        assert!(iq[..9_600].iter().all(|s| s.norm_sqr() == 0.0));
        assert!(iq[19_200..].iter().all(|s| s.norm_sqr() == 0.0));
    }

    #[test]
    fn kiwi_like_has_no_energy_outside_5_khz() {
        use manta_dsp::channelizer::Channelizer;
        let fs = KIWI_OUT_FS;
        let (iq_12k, _) = render_scene(&[], 12_000.0, 2.0, Some(VC1_NOISE_SEED_OTHER)).unwrap();
        let iq = kiwi_like_96k(&iq_12k, KIWI_RESIDUAL_DB, KIWI_RESIDUAL_SEED);
        assert_eq!(iq.len(), iq_12k.len() * KIWI_UP);

        let mut ch = Channelizer::new(fs, 0.0).unwrap();
        let hops = ch.process(&iq);
        let warmup = ch.filter_len() / ch.hop() + 1;
        let steady = &hops[warmup..];
        let n = ch.n_channels();
        let mut avg = vec![0.0f64; n];
        for hop in steady {
            for (k, &p) in hop.power.iter().enumerate() {
                avg[k] += p as f64 / steady.len() as f64;
            }
        }
        let (mut in_sum, mut in_n, mut worst_out, mut worst_f) = (0.0, 0usize, 0.0f64, 0.0);
        for (k, &p) in avg.iter().enumerate() {
            let f = ch.channel_freq_hz(k);
            if f.abs() <= 4_500.0 {
                in_sum += p;
                in_n += 1;
            } else if f.abs() > 5_500.0 && p > worst_out {
                worst_out = p;
                worst_f = f;
            }
        }
        let in_band = in_sum / in_n as f64;
        let rejection_db = 10.0 * (in_band / worst_out).log10();
        assert!(
            rejection_db >= 60.0,
            "channel at {worst_f} Hz only {rejection_db:.1} dB below in-band"
        );
    }

    #[test]
    fn dense_scene_drops_its_lead_in() {
        let (fs, duration_s) = (12_000.0, 1.0);
        let iq = dense_scene(fs, 300.0, duration_s, 1);
        assert_eq!(iq.len(), (duration_s * fs) as usize);
        assert_eq!(iq, dense_scene(fs, 300.0, duration_s, 1));
        // Mid-traffic from the first sample: keyed signals, not just noise.
        let p = iq.iter().map(|s| s.norm_sqr() as f64).sum::<f64>() / iq.len() as f64;
        assert!(p > 2.0 * (MASTER_SCALE as f64).powi(2), "power {p}");
    }

    #[test]
    fn existing_manifest_json_unchanged() {
        // The pre-MAN-312 manifest, field for field, in the same order.
        #[derive(serde::Serialize)]
        struct PreMan312Manifest<'a> {
            name: &'a str,
            fs: f64,
            duration_s: f64,
            center_freq_hz: f64,
            noise_seed: u64,
            expected_freq_hz: f64,
            expected_freqs_hz: &'a [f64],
            keyed_texts: &'a [String],
            generator: &'a str,
        }
        // Short V1: same code path as the 120 s vector.
        let spec = VectorSpec {
            duration_s: 2.0,
            ..v1()
        };
        let dir = tempfile::tempdir().unwrap();
        let m = write_fixture_set(&spec, dir.path()).unwrap();
        assert!(m.crashes.is_empty());
        let written = std::fs::read_to_string(dir.path().join("v1.manifest.json")).unwrap();
        assert!(!written.contains("crashes"), "{written}");
        let legacy = serde_json::to_string_pretty(&PreMan312Manifest {
            name: &m.name,
            fs: m.fs,
            duration_s: m.duration_s,
            center_freq_hz: m.center_freq_hz,
            noise_seed: m.noise_seed,
            expected_freq_hz: m.expected_freq_hz,
            expected_freqs_hz: &m.expected_freqs_hz,
            keyed_texts: &m.keyed_texts,
            generator: &m.generator,
        })
        .unwrap();
        assert_eq!(written, legacy);
        let back: Manifest = serde_json::from_str(&legacy).unwrap();
        assert!(back.crashes.is_empty());
    }

    #[test]
    fn crash_fixture_records_its_crashes() {
        let (spec, crashes) = vc2(40.0);
        let spec = VectorSpec {
            duration_s: 4.0,
            ..spec
        };
        let crashes: Vec<Crash> = crashes
            .into_iter()
            .filter(|c| c.start_s < spec.duration_s)
            .collect();
        assert_eq!(crashes.len(), 1);
        let dir = tempfile::tempdir().unwrap();
        let m = write_crash_fixture_set(&spec, &crashes, dir.path()).unwrap();
        assert_eq!(m.crashes, crashes);
        let json = std::fs::read_to_string(dir.path().join("vc2.manifest.json")).unwrap();
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.crashes, crashes);
        let mut src = manta_input::WavIqSource::open(&dir.path().join("vc2.wav")).unwrap();
        let wav = manta_input::read_all(&mut src).unwrap();
        assert_eq!(wav, render_with_crashes(&spec, &crashes).unwrap().samples);
        assert_ne!(wav, render(&spec).unwrap().samples);
    }
}
