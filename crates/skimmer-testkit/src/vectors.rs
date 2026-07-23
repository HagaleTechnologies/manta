//! Golden test vectors. SPEC §7: definitions live here
//! (module map §8: "§7 vectors -> skimmer-testkit::vectors").

use crate::keyer::Jitter;
use crate::scene::{render_scene, QsbSine, SignalSpec, WattersonFade};
use crate::wav::write_fixture;
use anyhow::Result;
use coppa_channel::watterson::WattersonPreset;
use num_complex::Complex32;
use std::path::Path;

/// A golden test vector's generation parameters. SPEC §7.
#[derive(Debug, Clone)]
pub struct VectorSpec {
    pub name: &'static str,
    pub fs: f64,
    pub duration_s: f64,
    pub center_freq_hz: f64,
    pub noise_seed: u64,
    pub signals: Vec<SignalSpec>,
}

/// SPEC §7 V1 "clean-20": 20 WPM, +20 dB, offset +12.34 kHz, W1AW,
/// AWGN only, no jitter. M0 = V1 passing end-to-end from a WAV file.
pub fn v1() -> VectorSpec {
    VectorSpec {
        name: "v1",
        fs: 96_000.0,
        duration_s: 120.0,
        center_freq_hz: 14_000_000.0,
        noise_seed: 0x534B_494D_5631, // "SKIMV1"
        signals: vec![SignalSpec {
            text: "CQ CQ DE W1AW W1AW K".into(),
            loop_text: true,
            wpm: 20.0,
            offset_hz: 12_340.0,
            snr_2500_db: 20.0,
            jitter: None,
            qsb: None,
            watterson: None,
            char_wpm: None,
        }],
    }
}

/// SPEC §7 V2 "fast-35": 35 WPM, +15 dB, JA1ABC, AWGN + 8% jitter.
pub fn v2() -> VectorSpec {
    VectorSpec {
        name: "v2",
        fs: 96_000.0,
        duration_s: 90.0,
        center_freq_hz: 14_000_000.0,
        noise_seed: 0x534B_494D_5632, // "SKIMV2"
        signals: vec![SignalSpec {
            text: "CQ CQ DE JA1ABC JA1ABC K".into(),
            loop_text: true,
            wpm: 35.0,
            offset_hz: -8_200.0,
            snr_2500_db: 15.0,
            jitter: Some(Jitter {
                sigma: 0.08,
                seed: 0x5632,
            }),
            qsb: None,
            watterson: None,
            char_wpm: None,
        }],
    }
}

/// SPEC §7 V3 "slow-weak": 12 WPM, +6 dB, VK9DX, AWGN + 8% jitter.
pub fn v3() -> VectorSpec {
    VectorSpec {
        name: "v3",
        fs: 96_000.0,
        duration_s: 120.0,
        center_freq_hz: 14_000_000.0,
        noise_seed: 0x534B_494D_5633, // "SKIMV3"
        signals: vec![SignalSpec {
            text: "CQ CQ DE VK9DX VK9DX K".into(),
            loop_text: true,
            wpm: 12.0,
            offset_hz: 5_600.0,
            snr_2500_db: 6.0,
            jitter: Some(Jitter {
                sigma: 0.08,
                seed: 0x5633,
            }),
            qsb: None,
            watterson: None,
            char_wpm: None,
        }],
    }
}

/// SPEC §7 V6 "qsb-sine": 20 WPM, K5ZZZ, AWGN, sinusoidal envelope QSB.
pub fn v6() -> VectorSpec {
    VectorSpec {
        name: "v6",
        fs: 96_000.0,
        duration_s: 120.0,
        center_freq_hz: 14_000_000.0,
        noise_seed: 0x534B_494D_5636, // "SKIMV6"
        signals: vec![SignalSpec {
            text: "CQ CQ DE K5ZZZ K5ZZZ K".into(),
            loop_text: true,
            wpm: 20.0,
            offset_hz: -15_000.0,
            snr_2500_db: 20.0, // peak SNR; QSB brings the trough toward ~0 dB
            jitter: None,
            qsb: Some(QsbSine { rate_hz: 0.2 }),
            watterson: None,
            char_wpm: None,
        }],
    }
}

/// SPEC §7 V4 "fade-good": 25 WPM, +10 dB, DL1ABC, Watterson CCIR-good.
pub fn v4() -> VectorSpec {
    VectorSpec {
        name: "v4",
        fs: 96_000.0,
        duration_s: 120.0,
        center_freq_hz: 14_000_000.0,
        noise_seed: 0x534B_494D_5634, // "SKIMV4"
        signals: vec![SignalSpec {
            text: "CQ CQ DE DL1ABC DL1ABC K".into(),
            loop_text: true,
            wpm: 25.0,
            offset_hz: 9_100.0,
            snr_2500_db: 10.0,
            jitter: None,
            qsb: None,
            watterson: Some(WattersonFade {
                preset: WattersonPreset::Good,
                seed: 0x5663,
            }),
            char_wpm: None,
        }],
    }
}

/// SPEC §7 V5 "fade-poor": 22 WPM, +3 dB, ZL2XYZ, Watterson CCIR-poor.
pub fn v5() -> VectorSpec {
    VectorSpec {
        name: "v5",
        fs: 96_000.0,
        duration_s: 120.0,
        center_freq_hz: 14_000_000.0,
        noise_seed: 0x534B_494D_5635, // "SKIMV5"
        signals: vec![SignalSpec {
            text: "CQ CQ DE ZL2XYZ ZL2XYZ K".into(),
            loop_text: true,
            wpm: 22.0,
            offset_hz: -11_300.0,
            snr_2500_db: 3.0,
            jitter: None,
            qsb: None,
            watterson: Some(WattersonFade {
                preset: WattersonPreset::Poor,
                seed: 0x5635,
            }),
            char_wpm: None,
        }],
    }
}

/// SPEC §7 V7 "adjacent": 24 WPM @ channel 107 (10,031.25 Hz) and 28 WPM @
/// channel 111 (10,406.25 Hz), both +15 dB, AWGN. Pass: exactly 2 tracks;
/// both char >= 95%; both freqs within ±15 Hz.
///
/// Deviates from SPEC §7's literal 150 Hz (~1.6 channels @
/// `CHANNEL_SPACING_HZ = 93.75` Hz, fs=96000/N=1024) separation: two
/// independently-keyed signals that close together fall inside each other's
/// ±1-channel ownership window (SPEC §2.5) and interleave hop-to-hop,
/// producing dozens of spurious garbled tracks (measured: 27) rather than 2
/// clean ones — empirically confirmed below this channelizer's ~2.5-channel
/// separation floor. This is fixture calibration, not a detector bug; see
/// `docs/DECISIONS/2026-07-19-m2-detector-track-pool-pins.md` (item 4).
/// Both offsets are bin-centered (exact channel multiples of 93.75 Hz) and 4
/// channels apart. An offline replica measured 0.0 Hz freq error on this
/// exact pair; a real in-pipeline run measured 11.0/1.5 Hz -- both
/// comfortably under the ±15 Hz criterion, but the gap vs. the replica's
/// prediction is attributed to the channelizer's known parabolic-
/// interpolation bias (deferred separately, see `Track::freq_hz`'s doc
/// comment), not to this fixture's separation.
pub fn v7() -> VectorSpec {
    const CHANNEL_SPACING_HZ: f64 = 93.75;
    VectorSpec {
        name: "v7",
        fs: 96_000.0,
        duration_s: 120.0,
        center_freq_hz: 14_000_000.0,
        noise_seed: 0x534B_494D_5637, // "SKIMV7"
        signals: vec![
            SignalSpec {
                text: "CQ CQ DE N1AA N1AA K".into(),
                loop_text: true,
                wpm: 24.0,
                offset_hz: 107.0 * CHANNEL_SPACING_HZ,
                snr_2500_db: 15.0,
                jitter: None,
                qsb: None,
                watterson: None,
                char_wpm: None,
            },
            SignalSpec {
                text: "CQ CQ DE N2BB N2BB K".into(),
                loop_text: true,
                wpm: 28.0,
                offset_hz: 111.0 * CHANNEL_SPACING_HZ,
                snr_2500_db: 15.0,
                jitter: None,
                qsb: None,
                watterson: None,
                char_wpm: None,
            },
        ],
    }
}

/// SPEC §7 V9 "drift": 18 WPM, +12 dB, drift +50 Hz/min, AWGN. Pass: 1
/// track (no split); char >= 90%; final freq within ±15 Hz of the drifted
/// end frequency.
///
/// `render_scene` has no built-in linear-drift primitive (only `offset_hz`,
/// a fixed NCO frequency) -- this vector approximates drift as a sequence
/// of short fixed-offset segments stepped every 2 s, each `render_scene`d
/// separately and concatenated, giving a staircase that closely
/// approximates linear drift at the channelizer's ~94 Hz channel
/// resolution (each 2 s step moves ~1.67 Hz, far under one channel).
pub fn v9() -> VectorSpec {
    VectorSpec {
        name: "v9",
        fs: 96_000.0,
        duration_s: 120.0,
        center_freq_hz: 14_000_000.0,
        noise_seed: 0x534B_494D_5639, // "SKIMV9"
        signals: vec![SignalSpec {
            text: "CQ CQ DE EA8AAA EA8AAA K".into(),
            loop_text: true,
            wpm: 18.0,
            offset_hz: 6_000.0, // start frequency; end = 6000 + 100 = 6100 Hz over 120s @ 50Hz/min
            snr_2500_db: 12.0,
            jitter: None,
            qsb: None,
            watterson: None,
            char_wpm: None,
        }],
    }
}

/// SPEC §7 V10 "farnsworth": 15 WPM effective / 25 WPM character speed,
/// +15 dB, AWGN. Pass: char >= 95%; word boundaries correct in steady
/// state (golden_v7_v9_v10.rs tolerates a small, documented warmup-floor
/// word-count drift during the Farnsworth gap-classifier's activation
/// bootstrap -- see `skimmer_decode::timing::FARNS_MIN_COUNT`'s doc
/// comment and the M2 sub-project 2 close-out pins doc).
pub fn v10() -> VectorSpec {
    VectorSpec {
        name: "v10",
        fs: 96_000.0,
        duration_s: 120.0,
        center_freq_hz: 14_000_000.0,
        noise_seed: 0x534B_494D_5610, // "SKIMV10" truncated to fit
        signals: vec![SignalSpec {
            text: "CQ CQ DE G4XXX G4XXX K".into(),
            loop_text: true,
            wpm: 15.0,
            offset_hz: 8_000.0,
            snr_2500_db: 15.0,
            jitter: None,
            qsb: None,
            watterson: None,
            char_wpm: Some(25.0),
        }],
    }
}

/// A rendered vector: samples, ground-truth keyed text per signal, expected
/// spot frequency. SPEC §7.
pub struct RenderedVector {
    pub samples: Vec<Complex32>,
    pub keyed_texts: Vec<String>,
    pub expected_freq_hz: f64,
}

/// Render a VectorSpec to samples + ground truth. SPEC §7.
pub fn render(spec: &VectorSpec) -> Result<RenderedVector> {
    let (samples, keyed_texts) = render_scene(
        &spec.signals,
        spec.fs,
        spec.duration_s,
        Some(spec.noise_seed),
    )?;
    Ok(RenderedVector {
        samples,
        keyed_texts,
        expected_freq_hz: spec.center_freq_hz + spec.signals[0].offset_hz,
    })
}

/// SPEC §7 V9: render a linear +50 Hz/min drift as a staircase of 2 s
/// fixed-offset segments (see `v9`'s doc comment). Returns the same shape
/// as `render`, with `expected_freq_hz` set to the *final* segment's
/// offset (SPEC's "final freq tracks within 15 Hz" pass criterion).
pub fn render_v9_drift(spec: &VectorSpec) -> Result<RenderedVector> {
    const DRIFT_HZ_PER_MIN: f64 = 50.0;
    const STEP_S: f64 = 2.0;
    let sig = &spec.signals[0];
    let n_steps = (spec.duration_s / STEP_S).round() as usize;
    let mut samples = Vec::new();
    let mut keyed_text = String::new();
    for i in 0..n_steps {
        let t_start_s = i as f64 * STEP_S;
        let offset_hz = sig.offset_hz + DRIFT_HZ_PER_MIN * t_start_s / 60.0;
        let step_sig = SignalSpec {
            offset_hz,
            ..sig.clone()
        };
        // Each step keys a fresh loop from t=0 (a small, deliberate
        // approximation: real drift wouldn't reset keying phase every
        // step). At 18 WPM a 2 s step only completes "CQ" before being cut
        // off and restarted, so the ground truth for the *whole* 120 s
        // scene is "CQ" repeated once per step, not one step's fragment --
        // caught empirically: using only a single mid-scene step's text as
        // ground truth against the full decoded transcript gave CER 58
        // (fragment vs full repeated transcript length mismatch). Since
        // every step is deterministic (same text/wpm/keyer, no jitter) and
        // starts fresh from t=0, all steps key identical text; concatenate
        // them all for the true full-scene ground truth.
        let (mut step_samples, step_texts) =
            render_scene(std::slice::from_ref(&step_sig), spec.fs, STEP_S, None)?;
        // `render_scene` unconditionally applies `MASTER_SCALE` once at the
        // end of every call, whether or not noise was added (it's meant to
        // be called exactly once per full render). Called per-step here,
        // that would scale each step down by `MASTER_SCALE` *before*
        // concatenation, and then noise+final-scale below would scale the
        // (already-scaled) signal down a second time while only scaling
        // the fresh noise once -- a ~26 dB SNR loss that silently zeroed
        // out detection (caught empirically: V9 bailed "no signal found").
        // Undo the premature per-step scaling so noise/scale below apply
        // exactly once across the whole concatenated drift, matching
        // `render`'s single-call scaling.
        for s in &mut step_samples {
            *s /= crate::scene::MASTER_SCALE;
        }
        samples.extend(step_samples);
        keyed_text.push_str(&step_texts[0]);
    }
    crate::noise::add_unit_awgn(&mut samples, spec.noise_seed);
    for s in &mut samples {
        *s *= crate::scene::MASTER_SCALE;
    }
    let final_offset_hz = sig.offset_hz + DRIFT_HZ_PER_MIN * (spec.duration_s - STEP_S) / 60.0;
    Ok(RenderedVector {
        samples,
        keyed_texts: vec![keyed_text],
        expected_freq_hz: spec.center_freq_hz + final_offset_hz,
    })
}

/// Sidecar recording a rendered fixture's generation parameters and ground
/// truth, for test assertions. Pinned decision 15.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Manifest {
    pub name: String,
    pub fs: f64,
    pub duration_s: f64,
    pub center_freq_hz: f64,
    pub noise_seed: u64,
    pub expected_freq_hz: f64,
    /// Per-signal expected absolute frequency, in `signals` order. For
    /// single-signal vectors this is `[expected_freq_hz]`.
    pub expected_freqs_hz: Vec<f64>,
    pub keyed_texts: Vec<String>,
    pub generator: String,
}

/// Write `<name>.wav`, `<name>.json`, `<name>.manifest.json` into `dir`.
/// Pinned decision 15.
pub fn write_fixture_set(spec: &VectorSpec, dir: &Path) -> Result<Manifest> {
    let rendered = render(spec)?;
    write_fixture(
        dir,
        spec.name,
        &rendered.samples,
        spec.fs,
        spec.center_freq_hz,
    )?;
    let expected_freqs_hz: Vec<f64> = spec
        .signals
        .iter()
        .map(|s| spec.center_freq_hz + s.offset_hz)
        .collect();
    let manifest = Manifest {
        name: spec.name.to_string(),
        fs: spec.fs,
        duration_s: spec.duration_s,
        center_freq_hz: spec.center_freq_hz,
        noise_seed: spec.noise_seed,
        expected_freq_hz: rendered.expected_freq_hz,
        expected_freqs_hz,
        keyed_texts: rendered.keyed_texts,
        generator: concat!("skimmer-testkit ", env!("CARGO_PKG_VERSION")).to_string(),
    };
    std::fs::write(
        dir.join(format!("{}.manifest.json", spec.name)),
        serde_json::to_string_pretty(&manifest)?,
    )?;
    Ok(manifest)
}

/// V9-specific fixture writer: same shape as `write_fixture_set`, using
/// `render_v9_drift` instead of `render`.
pub fn write_v9_fixture_set(spec: &VectorSpec, dir: &Path) -> Result<Manifest> {
    let rendered = render_v9_drift(spec)?;
    write_fixture(
        dir,
        spec.name,
        &rendered.samples,
        spec.fs,
        spec.center_freq_hz,
    )?;
    let manifest = Manifest {
        name: spec.name.to_string(),
        fs: spec.fs,
        duration_s: spec.duration_s,
        center_freq_hz: spec.center_freq_hz,
        noise_seed: spec.noise_seed,
        expected_freq_hz: rendered.expected_freq_hz,
        expected_freqs_hz: vec![rendered.expected_freq_hz],
        keyed_texts: rendered.keyed_texts,
        generator: concat!("skimmer-testkit ", env!("CARGO_PKG_VERSION")).to_string(),
    };
    std::fs::write(
        dir.join(format!("{}.manifest.json", spec.name)),
        serde_json::to_string_pretty(&manifest)?,
    )?;
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use skimmer_input::{IqSource, WavIqSource};

    #[test]
    fn v1_spec_matches_spec_table() {
        let v = v1();
        assert_eq!(v.fs, 96_000.0);
        assert_eq!(v.duration_s, 120.0);
        let s = &v.signals[0];
        assert_eq!(s.wpm, 20.0);
        assert_eq!(s.offset_hz, 12_340.0);
        assert_eq!(s.snr_2500_db, 20.0);
        assert!(s.jitter.is_none()); // V1: no jitter
        assert_eq!(s.text, "CQ CQ DE W1AW W1AW K");
    }

    #[test]
    fn v2_spec_matches_spec_table() {
        let v = v2();
        let s = &v.signals[0];
        assert_eq!(s.wpm, 35.0);
        assert_eq!(s.snr_2500_db, 15.0);
        assert!(s.jitter.is_some());
        assert_eq!(s.text, "CQ CQ DE JA1ABC JA1ABC K");
    }

    #[test]
    fn v3_spec_matches_spec_table() {
        let v = v3();
        let s = &v.signals[0];
        assert_eq!(s.wpm, 12.0);
        assert_eq!(s.snr_2500_db, 6.0);
        assert!(s.jitter.is_some());
        assert_eq!(s.text, "CQ CQ DE VK9DX VK9DX K");
    }

    #[test]
    fn v6_spec_matches_spec_table() {
        let v = v6();
        let s = &v.signals[0];
        assert_eq!(s.wpm, 20.0);
        let qsb = s.qsb.expect("V6 must carry a QsbSine spec");
        assert_eq!(qsb.rate_hz, 0.2);
    }

    #[test]
    fn v4_spec_matches_spec_table() {
        let v = v4();
        let s = &v.signals[0];
        assert_eq!(s.wpm, 25.0);
        assert_eq!(s.snr_2500_db, 10.0);
        assert!(s.watterson.is_some());
    }

    #[test]
    fn v5_spec_matches_spec_table() {
        let v = v5();
        let s = &v.signals[0];
        assert_eq!(s.wpm, 22.0);
        assert_eq!(s.snr_2500_db, 3.0);
        assert!(s.watterson.is_some());
    }

    #[test]
    fn fixture_roundtrips_through_wav() {
        // Short variant so the test stays fast; same code path as V1.
        let spec = VectorSpec {
            duration_s: 3.0,
            ..v1()
        };
        let rendered = render(&spec).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_fixture_set(&spec, dir.path()).unwrap();
        assert_eq!(manifest.expected_freq_hz, 14_012_340.0);

        let mut src = WavIqSource::open(&dir.path().join("v1.wav")).unwrap();
        assert_eq!(src.sample_rate(), 96_000.0);
        assert_eq!(src.center_freq_hz(), 14_000_000.0);
        let back = skimmer_input::read_all(&mut src).unwrap();
        assert_eq!(back, rendered.samples); // float32 WAV is lossless
    }
}
