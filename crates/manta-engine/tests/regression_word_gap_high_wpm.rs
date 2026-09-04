//! Regression for a high-WPM inter-WORD gap misclassification (MAN-2,
//! HagaleTechnologies/manta#11). Sibling of regression_char_gap_high_wpm.rs:
//! same Demod hysteresis+debounce mechanism (SPEC §3.3), hitting
//! WORD_GAP_DITS instead of CHAR_GAP_DITS. See
//! docs/DECISIONS/2026-09-04-word-gap-threshold-fix.md.
//!
//! Unverified against a live build: this repo could not be built in the
//! session that wrote this file (no network egress to fetch the pinned
//! coppa-dsp git revision -- see the decision doc's Environmental
//! constraint section). The six cases below are exactly the ticket's pinned
//! repros; `min_recall` is left at the plan's provisional floor (0.5)
//! because the Phase 4 measure-and-tighten step (run each case, take
//! measured_recall * 0.75) could not be executed. A build-capable session
//! should run these, confirm they are red on the pre-fix tree (WORD_GAP_DITS
//! = 5.0) and green after, and tighten `min_recall` per that rule.

use manta_engine::{decode_samples, PipelineConfig};
use manta_testkit::scene::{render_scene, SignalSpec};

/// Words in a decoded/keyed string, whitespace-normalized. V10's golden test
/// (crates/manta-cli/tests/golden_v7_v9_v10.rs) uses the same word-count
/// metric: CER is too blunt here, since a dropped space is only one edit
/// (manta_testkit::cer::cer whitespace-normalizes both sides).
fn words(s: &str) -> usize {
    s.split_whitespace().count()
}

struct Case {
    text: &'static str,
    wpm: f32,
    snr_2500_db: f32,
    offset_khz: i32,
    noise_seed: u64,
}

fn assert_word_boundaries_survive(c: Case, min_recall: f64) {
    let fs = 96_000.0;
    // 12 s: the roundtrip_iq.rs floor these cases were generated at -- past
    // SPEC §2.1's ~2.05 s warmup+confirm floor with several repetitions to
    // spare, without paying regression_char_gap_high_wpm.rs's 30 s cost six
    // times over.
    let duration_s = 12.0;
    let sig = SignalSpec {
        text: c.text.to_string(),
        loop_text: true,
        wpm: c.wpm,
        offset_hz: c.offset_khz as f64 * 1000.0,
        snr_2500_db: c.snr_2500_db,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
    };
    let (iq, texts) = render_scene(
        std::slice::from_ref(&sig),
        fs,
        duration_s,
        Some(c.noise_seed),
    )
    .unwrap();
    let report = decode_samples(&iq, fs, 0.0, &PipelineConfig::default()).unwrap();

    // 1. The historical signature: the two words must never appear fused.
    let fused = c.text.replace(' ', "");
    assert!(
        !report.text.contains(fused.as_str()),
        "words fused into {fused:?} (MAN-2) -- keyed {:?} decoded {:?}",
        texts[0],
        report.text
    );
    // 2. Word-boundary recall: the leading repetition(s) played during
    //    warmup+confirm are structurally lost (same floor documented in
    //    regression_char_gap_high_wpm.rs), so this is a recall gate, not
    //    an exact match.
    let recall = words(&report.text) as f64 / words(&texts[0]) as f64;
    assert!(
        recall >= min_recall,
        "word-boundary recall {recall:.3} < {min_recall} -- keyed {:?} decoded {:?}",
        texts[0],
        report.text
    );
}

#[test]
fn rn_xj0z_at_39wpm() {
    assert_word_boundaries_survive(
        Case {
            text: "RN XJ0Z",
            wpm: 39.4,
            snr_2500_db: 25.3,
            offset_khz: 14,
            noise_seed: 7281892873189538289,
        },
        0.5,
    );
}

#[test]
fn n6_lr3_at_35wpm() {
    assert_word_boundaries_survive(
        Case {
            text: "N6 LR3",
            wpm: 35.4,
            snr_2500_db: 28.6,
            offset_khz: 23,
            noise_seed: 860884949531190386,
        },
        0.5,
    );
}

#[test]
fn wk_q4w94_at_36wpm() {
    assert_word_boundaries_survive(
        Case {
            text: "WK Q4W94",
            wpm: 36.5,
            snr_2500_db: 29.5,
            offset_khz: -23,
            noise_seed: 12026420226686108211,
        },
        0.5,
    );
}

#[test]
fn vr_k2b_at_30wpm() {
    assert_word_boundaries_survive(
        Case {
            text: "VR K2B",
            wpm: 30.0,
            snr_2500_db: 29.5,
            offset_khz: 37,
            noise_seed: 4012477176760777842,
        },
        0.5,
    );
}

#[test]
fn dztx_p2pkwz_at_37wpm() {
    assert_word_boundaries_survive(
        Case {
            text: "DZTX P2PKWZ",
            wpm: 37.4,
            snr_2500_db: 28.7,
            offset_khz: 29,
            noise_seed: 15638026285272945288,
        },
        0.5,
    );
}

#[test]
fn vt_ru_at_32wpm() {
    assert_word_boundaries_survive(
        Case {
            text: "VT RU",
            wpm: 32.8,
            snr_2500_db: 28.4,
            offset_khz: -20,
            noise_seed: 9717735949415914133,
        },
        0.5,
    );
}
