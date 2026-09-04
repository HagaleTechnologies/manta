//! MAN-3 regression: a short (2-character), high-WPM text must never decode
//! to zero characters. The four tuples below are the ticket's own repro
//! cases, found by the 500-case threshold sweep behind
//! docs/DECISIONS/2026-07-18-char-gap-threshold-fix.md ("Known limitations",
//! item 2). Root cause and fix:
//! docs/DECISIONS/2026-09-04-man-3-short-high-wpm-zero-output.md.
//!
//! `duration_s` is pinned to 12.0 s for every case -- the exact value
//! `roundtrip_iq.rs::iq_roundtrip_with_noise` computes for these tuples
//! (`max(keyed_len/fs + 1.5, 12.0)`; each case's single-shot keyed length is
//! well under 1 s), i.e. the harness the cases were discovered with. The
//! ticket does not record the original sweep's duration; 12.0 s is the
//! reproducible stand-in, and the failure reproduced there at HEAD for
//! "Z5" (Err, zero events) and "DA" (fails at nearly every duration below
//! 10 s).

use manta_decode::events::DecoderEvent;
use manta_engine::{decode_samples, PipelineConfig};
use manta_testkit::scene::{render_scene, SignalSpec};

struct Case {
    text: &'static str,
    wpm: f32,
    snr_2500_db: f32,
    offset_hz: f64,
    seed: u64,
    /// Whether the *quality* assertion (decoded text contains the keyed
    /// 2-character text verbatim) applies. `"Z5"` is excluded: its one
    /// historically-observed success decoded garbled ("SZ"), a
    /// character-merge failure owned by MAN-5/MAN-6, not by MAN-3's
    /// zero-output scope. MAN-3's own criterion -- at least one
    /// CharDecoded, non-empty text -- is asserted for all four.
    expect_verbatim: bool,
}

const CASES: &[Case] = &[
    Case {
        text: "DA",
        wpm: 35.0,
        snr_2500_db: 28.3,
        offset_hz: 14_000.0,
        seed: 63032404875482,
        expect_verbatim: true,
    },
    Case {
        text: "VE",
        wpm: 39.1,
        snr_2500_db: 20.9,
        offset_hz: 1_000.0,
        seed: 685126706563701970,
        expect_verbatim: true,
    },
    Case {
        text: "Z5",
        wpm: 37.5,
        snr_2500_db: 29.9,
        offset_hz: -32_000.0,
        seed: 10751217158967957828,
        expect_verbatim: false,
    },
    Case {
        text: "D5",
        wpm: 34.1,
        snr_2500_db: 26.8,
        offset_hz: 19_000.0,
        seed: 10012388600385395947,
        expect_verbatim: true,
    },
];

const FS: f64 = 96_000.0;
const DURATION_S: f64 = 12.0;

fn run(case: &Case) -> (String, Vec<DecoderEvent>) {
    let sig = SignalSpec {
        text: case.text.to_string(),
        loop_text: true,
        wpm: case.wpm,
        offset_hz: case.offset_hz,
        snr_2500_db: case.snr_2500_db,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
    };
    let (iq, _texts) =
        render_scene(std::slice::from_ref(&sig), FS, DURATION_S, Some(case.seed)).unwrap();
    let report = decode_samples(&iq, FS, 0.0, &PipelineConfig::default()).unwrap_or_else(|e| {
        panic!(
            "{} @ {} WPM: decode_samples failed: {e}",
            case.text, case.wpm
        )
    });
    (report.text, report.events)
}

#[test]
fn short_high_wpm_texts_always_decode_at_least_one_character() {
    for case in CASES {
        let (text, events) = run(case);
        let chars = events
            .iter()
            .filter(|e| matches!(e, DecoderEvent::CharDecoded { .. }))
            .count();
        assert!(
            chars > 0,
            "{} @ {} WPM (snr {}, offset {} Hz, seed {}): zero CharDecoded events \
             across {} events -- MAN-3's exact symptom",
            case.text,
            case.wpm,
            case.snr_2500_db,
            case.offset_hz,
            case.seed,
            events.len()
        );
        assert!(
            !text.is_empty(),
            "{} @ {} WPM: decoded text is empty despite {chars} CharDecoded events -- \
             the reported track is not the track that decoded (see decode_samples' \
             select_report_track)",
            case.text,
            case.wpm
        );
    }
}

#[test]
fn short_high_wpm_texts_decode_their_keyed_text() {
    for case in CASES.iter().filter(|c| c.expect_verbatim) {
        let (text, _) = run(case);
        assert!(
            text.contains(case.text),
            "{} @ {} WPM: decoded {:?} does not contain the keyed text -- this case \
             decoded correctly at HEAD whenever a track survived, so a regression here \
             means the detector change lost decode quality, NOT that this bar is too high",
            case.text,
            case.wpm,
            text
        );
    }
}
