//! Golden test vectors. SPEC §7: definitions live here
//! (module map §8: "§7 vectors -> manta-testkit::vectors").

use crate::keyer::Jitter;
use crate::scene::{render_scene, QsbSine, SignalSpec, WattersonFade};
use crate::wav::write_fixture;
use anyhow::Result;
use coppa_channel::watterson::WattersonPreset;
use num_complex::Complex32;
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;
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
    /// Scene-wide raised-cosine rise/fall, silently overriding every
    /// signal's own `SignalSpec::rise_ms` at render time
    /// (`render`/`render_v9_drift`) -- there is currently no way to give
    /// two signals in the same vector different edge shapes; a future
    /// vector needing that must call `render_scene` directly instead of
    /// going through `VectorSpec`. SPEC v2 §8.2 VR4 ("hard-edges", 0.5 ms).
    /// 5.0 ms (standard) for every vector before VR4. `render`/
    /// `render_v9_drift` each carry a `debug_assert!` that every signal's
    /// `rise_ms` already matches this field, as a safety net against this
    /// override silently discarding a per-signal value someone set by
    /// mistake.
    pub rise_ms: f64,
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
            weight: 3.0,
            char_gap_units: 3.0,
            word_gap_units: 7.0,
            rise_ms: 5.0,
        }],
        rise_ms: 5.0,
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
            weight: 3.0,
            char_gap_units: 3.0,
            word_gap_units: 7.0,
            rise_ms: 5.0,
        }],
        rise_ms: 5.0,
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
            weight: 3.0,
            char_gap_units: 3.0,
            word_gap_units: 7.0,
            rise_ms: 5.0,
        }],
        rise_ms: 5.0,
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
            weight: 3.0,
            char_gap_units: 3.0,
            word_gap_units: 7.0,
            rise_ms: 5.0,
        }],
        rise_ms: 5.0,
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
            weight: 3.0,
            char_gap_units: 3.0,
            word_gap_units: 7.0,
            rise_ms: 5.0,
        }],
        rise_ms: 5.0,
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
            weight: 3.0,
            char_gap_units: 3.0,
            word_gap_units: 7.0,
            rise_ms: 5.0,
        }],
        rise_ms: 5.0,
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
                weight: 3.0,
                char_gap_units: 3.0,
                word_gap_units: 7.0,
                rise_ms: 5.0,
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
                weight: 3.0,
                char_gap_units: 3.0,
                word_gap_units: 7.0,
                rise_ms: 5.0,
            },
        ],
        rise_ms: 5.0,
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
            weight: 3.0,
            char_gap_units: 3.0,
            word_gap_units: 7.0,
            rise_ms: 5.0,
        }],
        rise_ms: 5.0,
    }
}

/// SPEC §7 V10 "farnsworth": 15 WPM effective / 25 WPM character speed,
/// +15 dB, AWGN. Pass: char >= 95%; word boundaries correct in steady
/// state (golden_v7_v9_v10.rs tolerates a small, documented warmup-floor
/// word-count drift during the Farnsworth gap-classifier's activation
/// bootstrap -- see `manta_decode::timing::FARNS_MIN_COUNT`'s doc
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
            weight: 3.0,
            char_gap_units: 3.0,
            word_gap_units: 7.0,
            rise_ms: 5.0,
        }],
        rise_ms: 5.0,
    }
}

/// Shared SPEC §7 V8/V8w scene: 50 signals, WPM 10..35, SNR (2500 Hz)
/// -2..25 dB, offsets uniform over +/-45 kHz with reject-redraw separation
/// (clear of the 1-channel/93.75 Hz merge threshold), 8% jitter. `v8w`
/// passes `Some(WattersonPreset::Poor)` to add CCIR-poor fading to every
/// signal on top of the identical AWGN-only scene `v8` uses -- same base
/// seed, so offsets/WPM/SNR/jitter are bit-identical between the two.
fn pileup_scene(name: &'static str, watterson: Option<WattersonPreset>) -> VectorSpec {
    const BASE_SEED: u64 = 0x534B_494D_5638; // "SKIMV8" -- shared by v8/v8w
    const MIN_SEPARATION_HZ: f64 = 300.0;
    let calls = crate::callsigns::pileup_calls();
    let mut rng = ChaCha8Rng::seed_from_u64(BASE_SEED);

    let mut offsets: Vec<f64> = Vec::with_capacity(50);
    'draw: while offsets.len() < 50 {
        let candidate = -45_000.0 + crate::u01(&mut rng) * 90_000.0;
        for &existing in &offsets {
            if (candidate - existing).abs() < MIN_SEPARATION_HZ {
                continue 'draw;
            }
        }
        offsets.push(candidate);
    }

    let signals = (0..50)
        .map(|i| SignalSpec {
            text: format!("CQ CQ DE {0} {0} K", calls[i]),
            loop_text: true,
            wpm: 10.0 + crate::u01(&mut rng) as f32 * 25.0,
            offset_hz: offsets[i],
            snr_2500_db: -2.0 + crate::u01(&mut rng) as f32 * 27.0,
            jitter: Some(Jitter {
                sigma: 0.08,
                seed: BASE_SEED ^ (i as u64 + 1),
            }),
            qsb: None,
            watterson: watterson.map(|preset| WattersonFade {
                preset,
                seed: BASE_SEED ^ 0xA5A5_0000 ^ (i as u64 + 1),
            }),
            char_wpm: None,
            weight: 3.0,
            char_gap_units: 3.0,
            word_gap_units: 7.0,
            rise_ms: 5.0,
        })
        .collect();

    VectorSpec {
        name,
        fs: 96_000.0,
        duration_s: 120.0,
        center_freq_hz: 14_000_000.0,
        noise_seed: BASE_SEED,
        signals,
        rise_ms: 5.0,
    }
}

/// SPEC §7 V8 "pileup-50": 50 signals, AWGN, jitter 8%.
pub fn v8() -> VectorSpec {
    pileup_scene("v8", None)
}

/// SPEC §7 V8w "pileup-50-fading": same scene as V8, Watterson CCIR-poor.
pub fn v8w() -> VectorSpec {
    pileup_scene("v8w", Some(WattersonPreset::Poor))
}

/// SPEC v2 §8.2 "real-conditions" vectors (VR1-VR8): conditions the
/// original V1-V10 vectors never exercised (contest speed, deep keying,
/// hard transmitter edges, co-channel occupancy, non-standard weighting,
/// tight Farnsworth-free spacing) -- the gap that let the real-signal
/// timing defect motivating this redesign go undetected. All: 120 s,
/// 96 kHz, 14 MHz center, text `CQ TEST <CALL> <CALL> TEST` unless stated
/// (SPEC v2 §8.2 preamble).
///
/// Shared shape for the single-signal vectors (VR1-VR4, VR6, VR7): +12.34
/// kHz offset, standard 3:1/3/7 keyer timing and 5 ms edges unless the
/// caller overrides a field afterward. VR5 (co-channel pair) builds its own
/// `VectorSpec` directly below.
fn single(
    name: &'static str,
    wpm: f32,
    snr_2500_db: f32,
    text: &str,
    noise_seed: u64,
    jitter: Option<Jitter>,
) -> VectorSpec {
    VectorSpec {
        name,
        fs: 96_000.0,
        duration_s: 120.0,
        center_freq_hz: 14_000_000.0,
        noise_seed,
        signals: vec![SignalSpec {
            text: text.to_string(),
            loop_text: true,
            wpm,
            offset_hz: 12_340.0,
            snr_2500_db,
            jitter,
            qsb: None,
            watterson: None,
            char_wpm: None,
            weight: 3.0,
            char_gap_units: 3.0,
            word_gap_units: 7.0,
            rise_ms: 5.0,
        }],
        rise_ms: 5.0,
    }
}

/// SPEC v2 §8.2 VR1 "contest-40": 40 WPM, +30 dB, W5AU, AWGN + 8% jitter.
/// Pass: char >= 95%; WPM 40 +/- 2; word boundaries >= 95%.
pub fn vr1() -> VectorSpec {
    single(
        "vr1",
        40.0,
        30.0,
        "CQ TEST W5AU W5AU TEST",
        0x5652_0001,
        Some(Jitter {
            sigma: 0.08,
            seed: 1,
        }),
    )
}

/// SPEC v2 §8.2 VR2 "contest-45": 45 WPM, +30 dB, K5TR, AWGN + 8% jitter.
/// Pass: char >= 90%; WPM 45 +/- 3.
pub fn vr2() -> VectorSpec {
    single(
        "vr2",
        45.0,
        30.0,
        "CQ TEST K5TR K5TR TEST",
        0x5652_0002,
        Some(Jitter {
            sigma: 0.08,
            seed: 2,
        }),
    )
}

/// SPEC v2 §8.2 VR3 "deep-keying": 30 WPM, +45 dB (~60 dB in channel),
/// N5ZZ, AWGN. Pass: char >= 98%; measured dit within +/-10% of 40 ms.
pub fn vr3() -> VectorSpec {
    single(
        "vr3",
        30.0,
        45.0,
        "CQ TEST N5ZZ N5ZZ TEST",
        0x5652_0003,
        None,
    )
}

/// SPEC v2 §8.2 VR4 "hard-edges": 32 WPM, +25 dB, 0.5 ms rise (click
/// sidebands), G3ABC, AWGN. Pass: char >= 90%.
pub fn vr4() -> VectorSpec {
    let mut v = single(
        "vr4",
        32.0,
        25.0,
        "CQ TEST G3ABC G3ABC TEST",
        0x5652_0004,
        None,
    );
    v.rise_ms = 0.5;
    v.signals[0].rise_ms = 0.5;
    v
}

/// SPEC v2 §8.2 VR5 "co-channel": two independently-keyed signals 30 Hz
/// apart -- well within one 93.75 Hz channelizer bin -- a genuine
/// co-channel occupancy test. A: 30 WPM +25 dB @ +10.000 kHz, "CQ TEST W1AA
/// W1AA TEST". B: 24 WPM +15 dB @ +10.030 kHz, "CQ TEST W2BB W2BB TEST".
/// Pass criteria (signal A only, per SPEC v2 §8.2): char >= 90%; 0 bogus
/// callsigns.
pub fn vr5() -> VectorSpec {
    VectorSpec {
        name: "vr5",
        fs: 96_000.0,
        duration_s: 120.0,
        center_freq_hz: 14_000_000.0,
        noise_seed: 0x5652_0005,
        signals: vec![
            SignalSpec {
                text: "CQ TEST W1AA W1AA TEST".into(),
                loop_text: true,
                wpm: 30.0,
                offset_hz: 10_000.0,
                snr_2500_db: 25.0,
                jitter: None,
                qsb: None,
                watterson: None,
                char_wpm: None,
                weight: 3.0,
                char_gap_units: 3.0,
                word_gap_units: 7.0,
                rise_ms: 5.0,
            },
            SignalSpec {
                text: "CQ TEST W2BB W2BB TEST".into(),
                loop_text: true,
                wpm: 24.0,
                offset_hz: 10_030.0,
                snr_2500_db: 15.0,
                jitter: None,
                qsb: None,
                watterson: None,
                char_wpm: None,
                weight: 3.0,
                char_gap_units: 3.0,
                word_gap_units: 7.0,
                rise_ms: 5.0,
            },
        ],
        rise_ms: 5.0,
    }
}

/// SPEC v2 §8.2 VR6 "weighting": 28 WPM, +20 dB, F6XYZ, AWGN, at two
/// non-standard dah:dit weights (2.6 and 3.4) exercising a classical
/// decoder's 3:1-tuned thresholds under real operator "weighting". `vr6a`
/// and `vr6b` share this scene, differing only in `weight`. Pass (both):
/// char >= 95%.
fn vr6(name: &'static str, weight: f32, noise_seed: u64) -> VectorSpec {
    let mut v = single(
        name,
        28.0,
        20.0,
        "CQ TEST F6XYZ F6XYZ TEST",
        noise_seed,
        None,
    );
    v.signals[0].weight = weight;
    v
}

/// VR6a: dah:dit 2.6.
pub fn vr6a() -> VectorSpec {
    vr6("vr6a", 2.6, 0x5652_0006)
}

/// VR6b: dah:dit 3.4.
pub fn vr6b() -> VectorSpec {
    vr6("vr6b", 3.4, 0x5652_0007)
}

/// SPEC v2 §8.2 VR7 "tight-spacing": 35 WPM, +20 dB, JA1ZZZ, AWGN, tight
/// (Farnsworth-free) spacing -- 2.5-dit character gaps and 5.0-dit word
/// gaps, vs. standard Morse's 3.0/7.0. Pass: word boundaries >= 90%; char
/// >= 95%.
pub fn vr7() -> VectorSpec {
    let mut v = single(
        "vr7",
        35.0,
        20.0,
        "CQ TEST JA1ZZZ JA1ZZZ TEST",
        0x5652_0008,
        None,
    );
    v.signals[0].char_gap_units = 2.5;
    v.signals[0].word_gap_units = 5.0;
    v
}

/// SPEC v2 §8.2 VR8 "qsb-fast": 30 WPM, +10 dB, DL9ABC, Watterson
/// CCIR-poor fading (same `WattersonFade` construction V5/V8w use -- see
/// `v5`/`pileup_scene` above). Pass: char >= 85%.
pub fn vr8() -> VectorSpec {
    let mut v = single(
        "vr8",
        30.0,
        10.0,
        "CQ TEST DL9ABC DL9ABC TEST",
        0x5652_0009,
        None,
    );
    v.signals[0].watterson = Some(WattersonFade {
        preset: WattersonPreset::Poor,
        seed: 0x5652_0009,
    });
    v
}

/// A rendered vector: samples, ground-truth keyed text per signal, expected
/// spot frequency. SPEC §7.
pub struct RenderedVector {
    pub samples: Vec<Complex32>,
    pub keyed_texts: Vec<String>,
    pub expected_freq_hz: f64,
}

/// Render a VectorSpec to samples + ground truth. SPEC §7. `spec.rise_ms`
/// overrides every signal's own `SignalSpec::rise_ms` (SPEC v2 §8.2 VR4).
pub fn render(spec: &VectorSpec) -> Result<RenderedVector> {
    for s in &spec.signals {
        debug_assert!(
            s.rise_ms == spec.rise_ms,
            "SignalSpec.rise_ms ({}) differs from VectorSpec.rise_ms ({}) -- \
             the vector-level value silently wins; if you need true \
             per-signal edge shapes, use render_scene directly instead of \
             VectorSpec::render()",
            s.rise_ms,
            spec.rise_ms
        );
    }
    let signals: Vec<SignalSpec> = spec
        .signals
        .iter()
        .map(|s| SignalSpec {
            rise_ms: spec.rise_ms,
            ..s.clone()
        })
        .collect();
    let (samples, keyed_texts) =
        render_scene(&signals, spec.fs, spec.duration_s, Some(spec.noise_seed))?;
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
    debug_assert!(
        sig.rise_ms == spec.rise_ms,
        "SignalSpec.rise_ms ({}) differs from VectorSpec.rise_ms ({}) -- \
         the vector-level value silently wins; if you need true per-signal \
         edge shapes, use render_scene directly instead of \
         VectorSpec::render_v9_drift()",
        sig.rise_ms,
        spec.rise_ms
    );
    let n_steps = (spec.duration_s / STEP_S).round() as usize;
    let mut samples = Vec::new();
    let mut keyed_text = String::new();
    for i in 0..n_steps {
        let t_start_s = i as f64 * STEP_S;
        let offset_hz = sig.offset_hz + DRIFT_HZ_PER_MIN * t_start_s / 60.0;
        let step_sig = SignalSpec {
            offset_hz,
            rise_ms: spec.rise_ms,
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
        generator: concat!("manta-testkit ", env!("CARGO_PKG_VERSION")).to_string(),
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
        generator: concat!("manta-testkit ", env!("CARGO_PKG_VERSION")).to_string(),
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
    use manta_input::{IqSource, WavIqSource};

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
    fn v8_spec_matches_spec_table() {
        let spec = v8();
        assert_eq!(spec.fs, 96_000.0);
        assert_eq!(spec.duration_s, 120.0);
        assert_eq!(spec.signals.len(), 50);
        for s in &spec.signals {
            assert!((10.0..=35.0).contains(&s.wpm), "wpm {} out of range", s.wpm);
            assert!(
                (-2.0..=25.0).contains(&s.snr_2500_db),
                "snr {} out of range",
                s.snr_2500_db
            );
            assert!(
                (-45_000.0..=45_000.0).contains(&s.offset_hz),
                "offset {} out of range",
                s.offset_hz
            );
            assert!(s.jitter.is_some(), "V8 must have 8% jitter");
            assert!(s.watterson.is_none(), "V8 is AWGN-only, no fading");
        }
        let unique_offsets: std::collections::BTreeSet<i64> =
            spec.signals.iter().map(|s| s.offset_hz as i64).collect();
        assert_eq!(unique_offsets.len(), 50, "all 50 offsets must be distinct");
    }

    #[test]
    fn v8w_spec_matches_v8_scene_plus_fading() {
        let v8 = v8();
        let v8w = v8w();
        assert_eq!(v8w.signals.len(), 50);
        for (a, b) in v8.signals.iter().zip(v8w.signals.iter()) {
            assert_eq!(a.offset_hz, b.offset_hz, "V8w must reuse V8's offsets");
            assert_eq!(a.wpm, b.wpm, "V8w must reuse V8's WPM");
            assert_eq!(a.snr_2500_db, b.snr_2500_db, "V8w must reuse V8's SNR");
            assert!(b.watterson.is_some(), "V8w must add Watterson fading");
        }
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
        let back = manta_input::read_all(&mut src).unwrap();
        assert_eq!(back, rendered.samples); // float32 WAV is lossless
    }
}
