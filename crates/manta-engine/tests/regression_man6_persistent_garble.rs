//! MAN-6 / HagaleTechnologies/manta#23 regression: persistent (non-converging)
//! garbled decode.
//!
//! Before the fix, this exact tuple decoded into a repeating "TT"/"TTT" stream
//! whose CER GREW with scene duration (5s 1.00, 8s 1.375, 12s 1.60, 20s 1.80)
//! instead of stabilizing. Root cause and fix:
//! docs/DECISIONS/2026-09-04-man6-leading-partial-run-and-badlock-recovery.md.
//!
//! The mechanism itself is covered hermetically and cheaply by
//! `manta-decode`'s `mid_element_start_does_not_lock_bad_timing`; this test
//! exists to prove it through the real channelizer + detector + track pool on
//! the tuple the ticket actually reported.
//!
//! Not run in this environment: `manta-dsp` pins `coppa-dsp` at a git
//! revision this sandbox cannot fetch (no network egress; see the
//! decisions doc's Measurements section), so this file could not be
//! compiled or executed here. It is written to the same contract as
//! `regression_char_gap_high_wpm.rs` and is expected to compile and pass
//! once run somewhere with network access to `coppa`; that run (and the
//! pre-fix CER values the second test's comment block calls for) is
//! recorded as outstanding in the decisions doc rather than fabricated.

use manta_engine::{decode_samples, PipelineConfig};
use manta_testkit::cer::cer;
use manta_testkit::scene::{render_scene, SignalSpec};

fn man6_spec() -> SignalSpec {
    SignalSpec {
        text: "AU".to_string(),
        loop_text: true,
        wpm: 18.117826,
        offset_hz: -20_000.0,
        snr_2500_db: 28.039232,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
    }
}

fn decode_at(duration_s: f64) -> (String, String) {
    let fs = 96_000.0;
    let sig = man6_spec();
    let (iq, texts) = render_scene(
        std::slice::from_ref(&sig),
        fs,
        duration_s,
        Some(2893936330082095u64),
    )
    .unwrap();
    let report = decode_samples(&iq, fs, 0.0, &PipelineConfig::default()).unwrap();
    (texts[0].clone(), report.text)
}

#[test]
fn man6_tuple_decodes_and_error_does_not_grow_with_duration() {
    let mut cers = Vec::new();
    for &d in &[12.0f64, 20.0, 40.0] {
        let (keyed, decoded) = decode_at(d);
        let c = cer(&keyed, &decoded);
        // The historical signature: a stream dominated by lone dahs.
        let t_frac = decoded.chars().filter(|&ch| ch == 'T').count() as f64
            / decoded
                .chars()
                .filter(|ch| !ch.is_whitespace())
                .count()
                .max(1) as f64;
        assert!(
            t_frac < 0.30,
            "{d}s: decode dominated by spurious 'T' ({t_frac:.2}) -- {decoded:?}"
        );
        assert!(
            c < 0.25,
            "{d}s: CER {c:.4}\nkeyed {keyed:?}\ndecoded {decoded:?}"
        );
        cers.push((d, c));
    }
    // Gherkin: "the decode error rate stabilizes or shrinks as a fraction of
    // the longer scene". The leading repetitions lost to the ~2.05 s
    // warmup+confirm floor are a fixed numerator over a growing denominator, so
    // CER must be non-increasing; 0.02 absorbs per-scene noise variation.
    for w in cers.windows(2) {
        assert!(
            w[1].1 <= w[0].1 + 0.02,
            "CER grew with duration: {:?} -> {:?}",
            w[0],
            w[1]
        );
    }
}

/// MAN-6 was a *phase* condition on the track-promotion hop, not a WPM or
/// offset band -- it can in principle fire wherever the AWGN realization
/// promotes a track mid-element. `roundtrip_iq.rs` is the natural home for that
/// coverage but stays `#[ignore]`d for unrelated reasons (#12, #22), so this
/// fixed grid samples the same space deterministically, excluding only
/// offset_hz == 0 (#12) and wpm in [10.0, 10.15] (#22).
///
/// Pre-fix CER measurement for this grid was not performable in this
/// environment (no network access to fetch the pinned `coppa` revision;
/// see this file's top comment) -- recorded as an open measurement task in
/// the decisions doc rather than fabricated here.
#[test]
fn man6_region_sweep_decodes_cleanly() {
    // (text, wpm, offset_khz, snr_db, seed) -- fixed, no proptest.
    const CASES: &[(&str, f32, i32, f32, u64)] = &[
        ("AU", 18.117826, -20, 28.039232, 2893936330082095),
        ("AU", 18.117826, 20, 28.039232, 11400330008812771),
        ("CQ", 12.5, -7, 22.0, 694100648224208083),
        ("K7X", 24.0, 31, 18.0, 4402998311021110331),
        ("AU", 33.14012, -13, 24.410885, 1200338874002998311),
        ("W1AW", 15.75, 4, 20.5, 88123400229981103),
    ];
    for &(text, wpm, offset_khz, snr, seed) in CASES {
        let fs = 96_000.0;
        let sig = SignalSpec {
            text: text.to_string(),
            loop_text: true,
            wpm,
            offset_hz: offset_khz as f64 * 1000.0,
            snr_2500_db: snr,
            jitter: None,
            qsb: None,
            watterson: None,
            char_wpm: None,
        };
        let (iq, texts) = render_scene(std::slice::from_ref(&sig), fs, 14.0, Some(seed)).unwrap();
        let report = decode_samples(&iq, fs, 0.0, &PipelineConfig::default()).unwrap();
        let c = cer(&texts[0], &report.text);
        assert!(
            c < 0.25,
            "{text} @ {wpm} wpm / {offset_khz} kHz / {snr} dB (seed {seed}): CER {c:.4}\n\
             keyed {:?}\ndecoded {:?}",
            texts[0],
            report.text
        );
    }
}
