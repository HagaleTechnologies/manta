//! MAN-31: operator suppression surfaces. Orthogonal to the automatic
//! validation pipeline (ARCHITECTURE §6 steps 1-4) -- manual overrides for
//! failure modes automatic validation doesn't catch.

use manta_decode::events::DecoderEvent;
use manta_decode::tree::Glyph;
use manta_spot::{Blocklist, NotchList, Spot, Validator};

const FS: f64 = 96_000.0;
const CTY_FIXTURE: &str = "\
United States:    5:  8: NA:  40.0:  75.0:  5.0:  K:
    K,W,N,AA,AB,AC;
";

fn word_events(track_id: u32, text: &str, start_ts: u64) -> (Vec<DecoderEvent>, u64) {
    let mut events = Vec::new();
    let mut ts = start_ts;
    for c in text.chars() {
        events.push(DecoderEvent::CharDecoded {
            track_id,
            sample_ts: ts,
            glyph: Glyph::Char(c),
            confidence: 0.95,
        });
        ts += 100;
    }
    events.push(DecoderEvent::WordBoundary {
        track_id,
        sample_ts: ts,
    });
    ts += 100;
    (events, ts)
}

fn transmission_events(track_id: u32, words: &[&str], start_ts: u64) -> Vec<DecoderEvent> {
    let mut events = Vec::new();
    let mut ts = start_ts;
    for word in words {
        let (mut w_events, next_ts) = word_events(track_id, word, ts);
        events.append(&mut w_events);
        ts = next_ts;
    }
    events
}

fn run(events: &[DecoderEvent], v: &mut Validator) -> Vec<Spot> {
    events.iter().flat_map(|e| v.ingest(e)).collect()
}

fn run_twice(v: &mut Validator, words: &[&str]) -> Vec<Spot> {
    let mut spots = run(&transmission_events(1, words, 0), v);
    spots.extend(run(&transmission_events(1, words, 100_000), v));
    spots
}

#[test]
fn v16_blocklisted_callsign_never_spots() {
    // Given a callsign is present in the operator's bad-call list
    let blocklist = Blocklist::parse("K5ARH\n");
    let mut v = Validator::new(FS, CTY_FIXTURE, None).with_blocklist(blocklist);
    let words = ["DE", "K5ARH", "K"];

    // When manta-spot would otherwise emit a spot for that callsign
    let spots = run_twice(&mut v, &words);

    // Then no spot is emitted
    assert!(
        spots.is_empty(),
        "blocklisted callsign must never be spotted, got {spots:?}"
    );
}

#[test]
fn v16_non_blocklisted_callsign_still_spots() {
    let blocklist = Blocklist::parse("W9ZZZ\n");
    let mut v = Validator::new(FS, CTY_FIXTURE, None).with_blocklist(blocklist);
    let words = ["DE", "K5ARH", "K"];

    let spots = run_twice(&mut v, &words);

    assert_eq!(
        spots.len(),
        1,
        "a callsign absent from the blocklist must spot normally, got {spots:?}"
    );
}

#[test]
fn v17_notched_frequency_never_spots() {
    // Given a frequency range is present in the operator's notched-frequency list
    let notch = NotchList::parse("14025000-14025100\n");
    let mut v = Validator::new(FS, CTY_FIXTURE, None).with_notch(notch);
    v.ingest(&DecoderEvent::TrackMeta {
        track_id: 1,
        snr_2500_db: 20.0,
        freq_hz: 14_025_050.0,
    });
    let words = ["DE", "K5ARH", "K"];

    // When a signal is detected within that frequency range
    let spots = run_twice(&mut v, &words);

    // Then no spot is emitted for that signal
    assert!(
        spots.is_empty(),
        "signal inside a notched range must never be spotted, got {spots:?}"
    );
}

/// ARCHITECTURE §8: "Every dropped/evicted/suppressed item is counted. No
/// silent loss anywhere in the pipeline." A blocklist/notch hit must be
/// distinguishable from an ordinary validation failure.
#[test]
fn suppressed_spots_are_counted_by_reason() {
    let blocklist = Blocklist::parse("K5ARH\n");
    let mut v = Validator::new(FS, CTY_FIXTURE, None).with_blocklist(blocklist);
    let words = ["DE", "K5ARH", "K"];
    // One transmission attempt -> one blocklist hit (the check runs on
    // every candidate word, ahead of the repetition gate).
    run(&transmission_events(1, &words, 0), &mut v);

    let counts = v.suppression_counts();
    assert_eq!(counts.blocklist, 1, "one blocklist hit should be counted");
    assert_eq!(counts.notch, 0);

    let notch = NotchList::parse("14025000-14025100\n");
    let mut v = Validator::new(FS, CTY_FIXTURE, None).with_notch(notch);
    v.ingest(&DecoderEvent::TrackMeta {
        track_id: 1,
        snr_2500_db: 20.0,
        freq_hz: 14_025_050.0,
    });
    run(&transmission_events(1, &words, 0), &mut v);

    let counts = v.suppression_counts();
    assert_eq!(counts.notch, 1, "one notch hit should be counted");
    assert_eq!(counts.blocklist, 0);
}

#[test]
fn v17_frequency_outside_notch_still_spots() {
    let notch = NotchList::parse("14025000-14025100\n");
    let mut v = Validator::new(FS, CTY_FIXTURE, None).with_notch(notch);
    v.ingest(&DecoderEvent::TrackMeta {
        track_id: 1,
        snr_2500_db: 20.0,
        freq_hz: 14_030_000.0,
    });
    let words = ["DE", "K5ARH", "K"];

    let spots = run_twice(&mut v, &words);

    assert_eq!(
        spots.len(),
        1,
        "a signal outside every notched range must spot normally, got {spots:?}"
    );
}
