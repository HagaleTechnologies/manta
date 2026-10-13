//! MAN-115: a signal keyed from the very start of a decode segment is
//! tracked and decoded without the old 2 s detector warm-up. The pre-roll
//! skip and the block-median floor seed (SPEC §2.2 "Startup") make a
//! 25 ms warm-up safe; these tests pin both halves of that claim: a real
//! signal promotes and decodes early, and pure noise still promotes
//! nothing. See docs/DECISIONS/2026-10-11-man115-startup-latency.md.

use manta_decode::events::DecoderEvent;
use manta_engine::{decode_samples, PipelineConfig};
use manta_testkit::scene::{render_scene, SignalSpec};

const FS: f64 = 96_000.0;
const CENTER_HZ: f64 = 14_000_000.0;

/// A clean 20 WPM, +20 dB looped CQ keyed from t = 0 (V1's parameters),
/// decoded with the default config, promotes exactly one track within
/// 0.25 s and decodes its first character from keying before 1 s. Before
/// MAN-115 promotion landed at ~2.06 s and the first character at
/// ~2.66 s (V1's leading "CQ " was lost to the warm-up).
#[test]
fn signal_keyed_at_segment_start_promotes_and_decodes_within_one_second() {
    let sig = SignalSpec {
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
    };
    let (iq, _texts) = render_scene(std::slice::from_ref(&sig), FS, 6.0, Some(1)).unwrap();
    let report = decode_samples(&iq, FS, CENTER_HZ, &PipelineConfig::default()).unwrap();

    let promoted: Vec<(u32, u64)> = report
        .events
        .iter()
        .filter_map(|e| match e {
            DecoderEvent::TrackPromoted {
                track_id,
                sample_ts,
                ..
            } => Some((*track_id, *sample_ts)),
            _ => None,
        })
        .collect();
    assert_eq!(
        promoted.len(),
        1,
        "expected exactly one promoted track, got {promoted:?}"
    );
    let (track_id, promoted_ts) = promoted[0];
    let promoted_s = promoted_ts as f64 / FS;
    assert!(
        promoted_s < 0.25,
        "TrackPromoted at {promoted_s:.4} s, expected < 0.25 s (pre-MAN-115: ~2.06 s)"
    );

    let first_char_ts = report
        .events
        .iter()
        .filter_map(|e| match e {
            DecoderEvent::CharDecoded {
                track_id: t,
                sample_ts,
                ..
            } if *t == track_id => Some(*sample_ts),
            _ => None,
        })
        .min()
        .expect("the promoted track decodes at least one character");
    let first_char_s = first_char_ts as f64 / FS;
    assert!(
        first_char_s < 1.0,
        "first CharDecoded at {first_char_s:.4} s, expected < 1.0 s (pre-MAN-115: ~2.66 s)"
    );
}

/// The other half: with the default 25 ms warm-up, pure noise from the
/// first hop of a segment promotes nothing. Before the pre-roll skip and
/// the block-median seed, a 0 ms warm-up promoted hundreds of noise tracks
/// off the near -140 dB start-up floor (MAN-172's `fast_w1aw` note).
#[test]
fn pure_noise_from_segment_start_promotes_nothing() {
    for seed in 1..=4u64 {
        let (iq, _) = render_scene(&[], FS, 4.0, Some(seed)).unwrap();
        // `decode_samples` reports a decode with no events at all as this
        // error; a 4 s input is far longer than one filter length, so it
        // means nothing was promoted. Any other outcome (an `Ok` report,
        // or the "only detector promotion events" error) means a noise
        // track was promoted.
        match decode_samples(&iq, FS, CENTER_HZ, &PipelineConfig::default()) {
            Err(e) if e.to_string() == "no signal found (no keyed CW signal in the input)" => {}
            Err(e) => panic!("noise seed {seed}: expected no promoted tracks, got: {e}"),
            Ok(report) => {
                let promoted = report
                    .events
                    .iter()
                    .filter(|e| matches!(e, DecoderEvent::TrackPromoted { .. }))
                    .count();
                panic!("noise seed {seed}: expected no promoted tracks, got {promoted}");
            }
        }
    }
}
