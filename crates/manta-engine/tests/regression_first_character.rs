//! MAN-199: a short transmission's first character must survive acquisition
//! warm-up. Before the lead-in, a promoted track's decoder started at the
//! promotion hop and every hop of real energy before it (energy onset to
//! candidate birth, then the 19-hop confirmation) was discarded, so `W1AW`
//! decoded as `M1AW` and `CQ` as `FQ`. Each scene here is noise, then the
//! text keyed once, then a silent tail: nothing is padded to make the text
//! longer than it is. See
//! docs/DECISIONS/2026-11-02-man199-lead-in-replay-and-one-close-routine.md.

use manta_engine::{decode_samples, PipelineConfig};
use manta_testkit::keyer::{key_text, KeyerSpec};
use manta_testkit::scene::{render_scene, SignalSpec};

const OFFSET_HZ: f64 = 5_000.0;
const LEAD_S: f64 = 3.0;
const TAIL_S: f64 = 4.0;
const LEAD_SEED: u64 = 11;

const TEXTS: [&str; 3] = ["W1AW", "CQ DE W1AW", "K1ABC"];
const WPMS: [f32; 5] = [20.0, 25.0, 30.0, 35.0, 40.0];

fn keyer(wpm: f32) -> KeyerSpec {
    KeyerSpec {
        wpm,
        char_wpm: None,
        rise_ms: 5.0,
        jitter: None,
        weight: 3.0,
        char_gap_units: 3.0,
        word_gap_units: 7.0,
        dit_units: 1.0,
        element_gap_units: 1.0,
    }
}

/// `LEAD_S` of noise, then `text` keyed once at `wpm`, then `TAIL_S` of
/// noise, decoded through the full pipeline at `fs`.
fn decode_once(text: &str, wpm: f32, snr: f32, seed: u64, fs: f64) -> Result<String, String> {
    let (env, _) = key_text(text, &keyer(wpm), fs).map_err(|e| e.to_string())?;
    let sig = SignalSpec {
        text: text.into(),
        loop_text: false,
        wpm,
        offset_hz: OFFSET_HZ,
        snr_2500_db: snr,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
        weight: 3.0,
        char_gap_units: 3.0,
        word_gap_units: 7.0,
        rise_ms: 5.0,
        dit_units: 1.0,
        element_gap_units: 1.0,
    };
    let (mut iq, _) = render_scene(&[], fs, LEAD_S, Some(LEAD_SEED)).map_err(|e| e.to_string())?;
    let keyed_s = env.len() as f64 / fs + TAIL_S;
    let (body, _) = render_scene(std::slice::from_ref(&sig), fs, keyed_s, Some(seed))
        .map_err(|e| e.to_string())?;
    iq.extend(body);
    decode_samples(&iq, fs, 0.0, &PipelineConfig::default())
        .map(|r| r.text.trim().to_string())
        .map_err(|e| e.to_string())
}

/// The 15-case grid at 20 dB: every case decodes its exact text, first
/// character included. Before MAN-199, 6 of 15 did (all five `W1AW` read
/// `M1AW`; `CQ DE W1AW` and `K1ABC` at 35 and 40 WPM lost their first
/// character).
#[test]
fn leading_character_survives_acquisition() {
    let mut wrong = Vec::new();
    for text in TEXTS {
        for wpm in WPMS {
            match decode_once(text, wpm, 20.0, 22, 96_000.0) {
                Ok(got) if got == text => {}
                other => wrong.push(format!("{text} @ {wpm} WPM -> {other:?}")),
            }
        }
    }
    assert!(wrong.is_empty(), "wrong decodes: {wrong:#?}");
}

/// The wider sweep: 5 SNRs x 3 seeds x the 15 cases, printing the
/// first-character and exact-text counts per SNR. The floor is the 64-hop
/// row of the decision record's table (45/45 first characters at every
/// SNR).
#[test]
#[ignore = "sweep: run with --release --ignored --nocapture"]
fn first_character_sweep() {
    for snr in [20.0f32, 12.0, 8.0, 5.0, 3.0] {
        let (mut first, mut exact, mut total) = (0, 0, 0);
        for seed in [22u64, 23, 24] {
            for text in TEXTS {
                for wpm in WPMS {
                    total += 1;
                    let got = decode_once(text, wpm, snr, seed, 96_000.0).unwrap_or_default();
                    if got.chars().next() == text.chars().next() {
                        first += 1;
                    }
                    if got == text {
                        exact += 1;
                    }
                }
            }
        }
        println!("SWEEP snr={snr} first_char={first}/{total} exact={exact}/{total}");
        assert_eq!(first, total, "first character at {snr} dB");
    }
}

/// Held-out texts, speeds and scenes (not used to pick the depth).
const HELD_OUT_TEXTS: [&str; 8] = [
    "TEST",
    "5NN TU",
    "EE",
    "R",
    "DX DE N0CALL",
    "73",
    "QRZ",
    "TNX",
];
const HELD_OUT_WPMS: [f32; 4] = [18.0, 28.0, 38.0, 45.0];
/// (snr_2500_db, offset_hz, seed)
const HELD_OUT_SCENES: [(f32, f64, u64); 3] = [
    (15.0, -7_000.0, 31),
    (6.0, 2_500.0, 32),
    (10.0, 11_000.0, 33),
];

/// Held-out cases that decoded their exact text before MAN-199 (measured on
/// the branch with `LEAD_IN_HOPS = 0`, which reproduces the pre-change
/// grid; 20 of 96 at 96 kS/s, 25 of 96 at 48 kS/s): (fs, text, wpm, scene
/// index).
const BASELINE_EXACT: &[(u32, &str, u32, usize)] = &[
    (96000, "TEST", 18, 0),
    (96000, "TEST", 18, 1),
    (96000, "TEST", 18, 2),
    (96000, "DX DE N0CALL", 18, 0),
    (96000, "DX DE N0CALL", 18, 1),
    (96000, "DX DE N0CALL", 18, 2),
    (96000, "DX DE N0CALL", 28, 0),
    (96000, "73", 18, 0),
    (96000, "73", 18, 1),
    (96000, "73", 18, 2),
    (96000, "QRZ", 18, 0),
    (96000, "QRZ", 18, 1),
    (96000, "QRZ", 18, 2),
    (96000, "QRZ", 28, 0),
    (96000, "QRZ", 28, 1),
    (96000, "QRZ", 28, 2),
    (96000, "QRZ", 38, 0),
    (96000, "TNX", 18, 0),
    (96000, "TNX", 18, 1),
    (96000, "TNX", 18, 2),
    (48000, "TEST", 18, 0),
    (48000, "TEST", 18, 1),
    (48000, "TEST", 18, 2),
    (48000, "DX DE N0CALL", 18, 0),
    (48000, "DX DE N0CALL", 18, 1),
    (48000, "DX DE N0CALL", 18, 2),
    (48000, "DX DE N0CALL", 28, 0),
    (48000, "73", 18, 0),
    (48000, "73", 18, 1),
    (48000, "73", 18, 2),
    (48000, "73", 28, 0),
    (48000, "QRZ", 18, 0),
    (48000, "QRZ", 18, 1),
    (48000, "QRZ", 18, 2),
    (48000, "QRZ", 28, 0),
    (48000, "QRZ", 28, 1),
    (48000, "QRZ", 28, 2),
    (48000, "QRZ", 38, 0),
    (48000, "QRZ", 38, 1),
    (48000, "QRZ", 38, 2),
    (48000, "QRZ", 45, 0),
    (48000, "TNX", 18, 0),
    (48000, "TNX", 18, 1),
    (48000, "TNX", 18, 2),
    (48000, "TNX", 28, 0),
];

fn decode_held_out(text: &str, wpm: f32, scene: usize, fs: f64) -> String {
    let (snr, offset_hz, seed) = HELD_OUT_SCENES[scene];
    let (env, _) = key_text(text, &keyer(wpm), fs).unwrap();
    let sig = SignalSpec {
        text: text.into(),
        loop_text: false,
        wpm,
        offset_hz,
        snr_2500_db: snr,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
        weight: 3.0,
        char_gap_units: 3.0,
        word_gap_units: 7.0,
        rise_ms: 5.0,
        dit_units: 1.0,
        element_gap_units: 1.0,
    };
    let (mut iq, _) = render_scene(&[], fs, LEAD_S, Some(LEAD_SEED)).unwrap();
    let (body, _) = render_scene(
        std::slice::from_ref(&sig),
        fs,
        env.len() as f64 / fs + TAIL_S,
        Some(seed),
    )
    .unwrap();
    iq.extend(body);
    decode_samples(&iq, fs, 0.0, &PipelineConfig::default())
        .map(|r| r.text.trim().to_string())
        .unwrap_or_default()
}

/// The held-out set at 96 and 48 kS/s: no case that decoded exactly before
/// MAN-199 decodes wrong with the lead-in. Prints every case and the
/// per-rate exact counts.
#[test]
#[ignore = "sweep: run with --release --ignored --nocapture"]
fn held_out_cases_exact_at_baseline_stay_exact() {
    let mut regressed = Vec::new();
    for fs in [96_000u32, 48_000] {
        let mut exact = 0;
        for text in HELD_OUT_TEXTS {
            for wpm in HELD_OUT_WPMS {
                for scene in 0..HELD_OUT_SCENES.len() {
                    let got = decode_held_out(text, wpm, scene, f64::from(fs));
                    let ok = got == text;
                    if ok {
                        exact += 1;
                        println!("HELD_EXACT ({fs}, {text:?}, {wpm}, {scene}),");
                    }
                    if !ok && BASELINE_EXACT.contains(&(fs, text, wpm as u32, scene)) {
                        regressed.push(format!("{fs} {text} {wpm} WPM scene {scene} -> {got:?}"));
                    }
                }
            }
        }
        println!("HELD fs={fs} exact={exact}/96");
    }
    assert!(
        regressed.is_empty(),
        "exact at baseline, wrong now: {regressed:#?}"
    );
}
