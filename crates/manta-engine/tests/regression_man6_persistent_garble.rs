//! MAN-6 / HagaleTechnologies/manta#23 regression: persistent (non-converging)
//! garbled decode.
//!
//! On the September base, the ticket's tuple decoded into a repeating
//! "TT"/"TTT" stream whose CER GREW with scene duration (5s 1.00, 8s 1.375,
//! 12s 1.60, 20s 1.80). Decode-core v2 and the later timing work fixed that
//! tuple and the plan's whole six-case sweep; `SpeedTracker`'s bad-lock
//! recovery (both branches) still fixes dit-dominated text ("HH", "5NN") that
//! otherwise locks wrong forever. Mechanism and both measurement rounds:
//! docs/DECISIONS/2026-09-04-man6-leading-partial-run-and-badlock-recovery.md.
//!
//! The mechanism itself is covered hermetically and cheaply in
//! `manta-decode`'s `timing.rs` tests; this file proves it through the real
//! channelizer + detector + track pool.

use manta_engine::{decode_samples, PipelineConfig};
use manta_testkit::cer::cer;
use manta_testkit::scene::{render_scene, SignalSpec};

fn spec(text: &str, wpm: f32, offset_khz: f64, snr_2500_db: f32) -> SignalSpec {
    SignalSpec {
        text: text.to_string(),
        loop_text: true,
        wpm,
        offset_hz: offset_khz * 1000.0,
        snr_2500_db,
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
    }
}

fn decode(sig: &SignalSpec, duration_s: f64, seed: u64) -> (String, String) {
    let fs = 96_000.0;
    let (iq, texts) = render_scene(std::slice::from_ref(sig), fs, duration_s, Some(seed)).unwrap();
    let report = decode_samples(&iq, fs, 0.0, &PipelineConfig::default()).unwrap();
    (texts[0].clone(), report.text)
}

/// The plan's original six-case sweep, including the ticket's own tuple and
/// the two cases the first round had to drop (+20 kHz silence, CQ garble).
/// All decode with only the structural leading-element loss on this base.
#[test]
fn man6_region_sweep_decodes_cleanly() {
    // (text, wpm, offset_khz, snr_db, seed) -- fixed, no proptest.
    const CASES: &[(&str, f32, f64, f32, u64)] = &[
        ("AU", 18.117826, -20.0, 28.039232, 2893936330082095),
        ("AU", 18.117826, 20.0, 28.039232, 11400330008812771),
        ("CQ", 12.5, -7.0, 22.0, 694100648224208083),
        ("K7X", 24.0, 31.0, 18.0, 4402998311021110331),
        ("AU", 33.14012, -13.0, 24.410885, 1200338874002998311),
        ("W1AW", 15.75, 4.0, 20.5, 88123400229981103),
    ];
    for &(text, wpm, offset_khz, snr, seed) in CASES {
        let (keyed, decoded) = decode(&spec(text, wpm, offset_khz, snr), 14.0, seed);
        let c = cer(&keyed, &decoded);
        assert!(
            c < 0.25,
            "{text} @ {wpm} wpm / {offset_khz} kHz / {snr} dB (seed {seed}): CER {c:.4}\n\
             keyed {keyed:?}\ndecoded {decoded:?}"
        );
    }
}

/// The ticket's own tuple: error must shrink, not grow, as the scene
/// lengthens -- the ticket's core complaint.
#[test]
fn man6_tuple_error_does_not_grow_with_duration() {
    let sig = spec("AU", 18.117826, -20.0, 28.039232);
    let seed = 2893936330082095;
    let mut prev = f64::INFINITY;
    for d in [12.0, 20.0, 40.0] {
        let (keyed, decoded) = decode(&sig, d, seed);
        let c = cer(&keyed, &decoded) as f64;
        assert!(c < 0.25, "{d}s: CER {c:.4} -- {decoded:?}");
        assert!(
            c <= prev,
            "{d}s: CER grew ({prev:.4} -> {c:.4}) -- {decoded:?}"
        );
        prev = c;
    }
}

/// Dit-dominated tuples that lock wrong forever without the bad-lock
/// recovery (measured on main without it, 14 s / 40 s CER: 1.25 / 1.07,
/// 3.08 / 3.00, 2.83 / 2.77). With it, the garbled run is a bounded prefix
/// and the keyed word then decodes word-bounded.
#[test]
fn dit_dominated_bad_lock_converges() {
    const CASES: &[(&str, f32, f64, f32, u64)] = &[
        ("HH", 22.67, 29.31, 23.33, 8986880970975364820),
        ("HH", 19.0, 20.25, 19.63, 2041354727252151524),
        ("5NN", 16.97, -35.43, 18.19, 9855354123770958939),
    ];
    for &(text, wpm, offset_khz, snr, seed) in CASES {
        let sig = spec(text, wpm, offset_khz, snr);
        let (k14, d14) = decode(&sig, 14.0, seed);
        let (k40, d40) = decode(&sig, 40.0, seed);
        let (c14, c40) = (cer(&k14, &d14), cer(&k40, &d40));
        assert!(
            c40 < 0.5 && c40 < c14,
            "{text} @ {wpm} wpm / {offset_khz} kHz: did not converge (CER {c14:.4} -> {c40:.4})\n\
             40s decoded {d40:?}"
        );
        let words = d40.split_whitespace().filter(|&w| w == text).count();
        assert!(
            words >= 5,
            "{text} @ {wpm} wpm / {offset_khz} kHz: no clean, word-bounded recovery -- {d40:?}"
        );
    }
}
