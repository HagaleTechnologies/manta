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
