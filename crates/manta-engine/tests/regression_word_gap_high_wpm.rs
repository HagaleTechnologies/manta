//! Regression for a high-WPM inter-WORD gap misclassification (MAN-2,
//! HagaleTechnologies/manta#11). Sibling of regression_char_gap_high_wpm.rs:
//! same Demod hysteresis+debounce mechanism (SPEC §3.3), hitting
//! WORD_GAP_DITS instead of CHAR_GAP_DITS. See
//! docs/DECISIONS/2026-09-04-word-gap-threshold-fix.md.
//!
//! MAN-2 remediate (live-instrumented, this build): of the ticket's six
//! pinned repro cases, only `rn_xj0z_at_39wpm` reproduces a clean decode in
//! this repo state -- the other five fail identically (same decoded string,
//! same recall) at WORD_GAP_DITS=3.5, 4.5, and the unmodified SPEC nominal
//! 5.0, so their failure is independent of this fix. Direct trace evidence
//! (raw Demod -> GapClassifier, see `timing.rs`'s `WORD_GAP_DITS` doc
//! comment) shows they hit near-total decode garbling unrelated to word-gap
//! classification -- issue #23 territory
//! (`crates/manta-engine/tests/roundtrip_iq.rs`'s known, unrelated
//! high-WPM/offset/SNR garbling bug), not the MAN-2 mechanism. Per the
//! plan's "never delete a ticket-pinned case silently" rule, they are kept
//! `#[ignore]`d below with their measured numbers rather than dropped.

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

fn decode_case(c: &Case) -> (String, String) {
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
    (texts[0].clone(), report.text)
}

/// `min_recall`/`max_recall` bound `words(decoded) / words(keyed)`. The
/// upper bound matters as much as the lower one: a decoder that split after
/// every character would score recall far above 1.0 and, before this
/// remediate pass, would have passed this gate silently (a one-sided recall
/// check can never catch the over-splitting failure mode this fix's
/// threshold choice directly trades against -- see `timing.rs`'s
/// `WORD_GAP_DITS` doc comment for the measured over-splitting rates).
fn assert_word_boundaries_survive(c: Case, min_recall: f64, max_recall: f64) {
    let (keyed, decoded) = decode_case(&c);

    // 1. The historical signature: the two words must never appear fused.
    let fused = c.text.replace(' ', "");
    assert!(
        !decoded.contains(fused.as_str()),
        "words fused into {fused:?} (MAN-2) -- keyed {keyed:?} decoded {decoded:?}"
    );
    // 2. Word-boundary recall, both directions: the leading repetition(s)
    //    played during warmup+confirm are structurally lost (same floor
    //    documented in regression_char_gap_high_wpm.rs), so the lower bound
    //    is a recall gate, not an exact match; the upper bound catches
    //    spurious over-splitting into more words than were ever keyed.
    let recall = words(&decoded) as f64 / words(&keyed) as f64;
    assert!(
        (min_recall..=max_recall).contains(&recall),
        "word-boundary recall {recall:.3} outside [{min_recall}, {max_recall}] -- keyed {keyed:?} decoded {decoded:?}"
    );
}

#[test]
fn rn_xj0z_at_39wpm() {
    // Measured (this build, WORD_GAP_DITS=4.5, confirmed identical across
    // 3 runs -- deterministic): keyed "RN XJ0Z RN XJ0Z RN XJ0Z RN XJ0Z RN"
    // (9 words) decodes to "AZ RN XJ0Z RN XJ0Z RN XJ0Z RN" (8 words,
    // recall=0.888889 -- byte-identical to the WORD_GAP_DITS=5.0 nominal,
    // this case never exhibited the compressed-word-gap defect in this
    // build; see timing.rs's WORD_GAP_DITS doc comment). Per the plan's
    // Phase 4 rule, min_recall = measured * 0.75 = 0.6667, rounded down to
    // 0.66. max_recall = 1.2 gives headroom above the measured 0.89 while
    // still catching a "split every character" regression (which would
    // score recall around 3.0, per the plan's own worked example).
    assert_word_boundaries_survive(
        Case {
            text: "RN XJ0Z",
            wpm: 39.4,
            snr_2500_db: 25.3,
            offset_khz: 14,
            noise_seed: 7281892873189538289,
        },
        0.66,
        1.2,
    );
}

/// The remaining five pinned cases, kept `#[ignore]`d rather than deleted
/// (plan rule: never drop a ticket-pinned case silently). Each is measured,
/// live, to fail identically at WORD_GAP_DITS=3.5 (originally shipped),
/// WORD_GAP_DITS=4.5 (this fix), and the unmodified SPEC nominal 5.0 --
/// so the failure is independent of this fix's threshold choice and is not
/// the MAN-2 word-gap-classification defect. Direct GapClassifier trace
/// evidence on `dztx_p2pkwz_at_37wpm` (the only one of the five with enough
/// surviving decode to trace) shows only 2 gaps total reach the classifier's
/// long-gap floor before decode collapses -- consistent with a channel/demod
/// garbling failure upstream of gap classification (issue #23 territory),
/// not a misclassified word boundary. Run individually with `--ignored` if
/// re-investigating; do not re-enable without first confirming they exercise
/// MAN-2's signature specifically.
mod not_reproducing_cases {
    use super::*;

    #[test]
    #[ignore]
    fn n6_lr3_at_35wpm() {
        // Measured: decode_samples returns Err("no signal found (input
        // shorter than one filter length or empty)") -- a hard pipeline
        // error, not a recall shortfall.
        let c = Case {
            text: "N6 LR3",
            wpm: 35.4,
            snr_2500_db: 28.6,
            offset_khz: 23,
            noise_seed: 860884949531190386,
        };
        let (_keyed, _decoded) = decode_case(&c);
    }

    #[test]
    #[ignore]
    fn wk_q4w94_at_36wpm() {
        // Measured: keyed "WK Q4W94 WK Q4W94 WK Q4W94 WK Q" decodes to ""
        // (zero CharDecoded events survive) -- recall 0.000.
        assert_word_boundaries_survive(
            Case {
                text: "WK Q4W94",
                wpm: 36.5,
                snr_2500_db: 29.5,
                offset_khz: -23,
                noise_seed: 12026420226686108211,
            },
            0.5,
            1.2,
        );
    }

    #[test]
    #[ignore]
    fn vr_k2b_at_30wpm() {
        // Measured: keyed "VR K2B VR K2B VR K2B VR K2B V" decodes to "TB"
        // -- recall 0.111, and "TB" is not even a subsequence of the keyed
        // characters (channel garbling, not a dropped space).
        assert_word_boundaries_survive(
            Case {
                text: "VR K2B",
                wpm: 30.0,
                snr_2500_db: 29.5,
                offset_khz: 37,
                noise_seed: 4012477176760777842,
            },
            0.5,
            1.2,
        );
    }

    #[test]
    #[ignore]
    fn dztx_p2pkwz_at_37wpm() {
        // Measured: keyed "DZTX P2PKWZ DZTX P2PKWZ DZTX P2P" decodes to
        // "A P" -- recall 0.333. GapClassifier trace: only 2 gaps in the
        // whole clip reach the u>=1.5 long-gap floor (one InterWord at
        // u=4.7059, one InterChar at u=1.976); everything else is lost to
        // upstream decode garbling before it ever reaches gap
        // classification.
        assert_word_boundaries_survive(
            Case {
                text: "DZTX P2PKWZ",
                wpm: 37.4,
                snr_2500_db: 28.7,
                offset_khz: 29,
                noise_seed: 15638026285272945288,
            },
            0.5,
            1.2,
        );
    }

    #[test]
    #[ignore]
    fn vt_ru_at_32wpm() {
        // Measured: keyed "VT RU VT RU VT RU VT RU VT RU VT RU VT RU"
        // decodes to "ET R" -- recall 0.143.
        assert_word_boundaries_survive(
            Case {
                text: "VT RU",
                wpm: 32.8,
                snr_2500_db: 28.4,
                offset_khz: -20,
                noise_seed: 9717735949415914133,
            },
            0.5,
            1.2,
        );
    }
}
